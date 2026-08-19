//! Alarm ladder state machine (pure).
//!
//! Priority order, highest first: Fault > NearLimit > SustainedHot > Normal.
//! Escalation to a higher-priority state is immediate. De-escalation requires
//! the active state's trigger to be continuously false for `cooldown_secs`,
//! at which point the machine re-evaluates from the top of the states below
//! the one that just cleared.

pub use crate::config::AlertsConfig;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LedCommand {
    Breathing {
        color: (u8, u8, u8),
        speed_idx: u8,
    },
    Runway {
        colors: [(u8, u8, u8); 2],
        speed_idx: u8,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct AlarmInputs {
    pub control_temp: Option<f64>,
    pub duty: u8,
    /// Total sensor loss this tick (highest-priority condition: Fault).
    pub fallback_active: bool,
}

/// Multi-signal thermal context, layered onto `AlarmInputs` via `tick_with`
/// rather than added as new `AlarmInputs` fields — `AlarmInputs` is used via
/// inline struct literals in the pre-existing test suite, and any new field
/// there would break all of them. `Default` (both `None`/`false`) is exactly
/// the legacy behavior: `tick()` delegates to `tick_with` with defaults, so
/// callers that never learned about GPU-reported margin or throttle state
/// see no change at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThermalHints {
    /// Headroom to the card's own reported limit. `None` = unavailable —
    /// must NEVER trigger NearLimit, or a card that doesn't report headroom
    /// would sit permanently in NearLimit.
    pub thermal_margin_c: Option<f64>,
    /// A thermal throttle reason is active or still within its hold window.
    /// This does NOT map to Fault: Fault means "lost sensors, flying blind"
    /// and has its own LED signature; thermal throttling is a normal (if
    /// urgent) operating condition the card itself is already responding
    /// to, so it only raises the NearLimit rung.
    pub thermal_throttling: bool,
}

fn tuple3(a: [u8; 3]) -> (u8, u8, u8) {
    (a[0], a[1], a[2])
}

fn state_name(state: u8) -> &'static str {
    match state {
        1 => "SustainedHot",
        2 => "NearLimit",
        3 => "Fault",
        _ => "Normal",
    }
}

pub struct AlarmMachine {
    enabled: bool,
    sustained_hot_c: f64,
    sustained_after_secs: u64,
    near_limit_temp: f64,
    /// Same knob as `near_limit_temp` (`cfg.near_limit_margin_c`), reused
    /// rather than a new config field — "within N degrees of the limit" is
    /// semantically identical whether measured against the curve's own top
    /// temperature or the card's self-reported headroom.
    near_limit_margin_c: f64,
    escalate_every_secs: u64,
    cooldown_secs: u64,
    alert_color: (u8, u8, u8),
    fault_colors: [(u8, u8, u8); 2],

    // State + timers, all in seconds.
    state: u8,
    /// Continuous seconds control_temp >= sustained_hot_c has held. Freezes
    /// (neither accumulates nor resets) on control_temp == None ticks;
    /// resets to 0 the instant control_temp is Some but below threshold.
    hot_secs: u64,
    /// Seconds spent continuously in the current displayed state.
    in_state_secs: u64,
    /// Seconds the current state's own trigger has been continuously false;
    /// once this reaches cooldown_secs the state is dropped.
    clear_secs: u64,
}

impl AlarmMachine {
    pub fn new(cfg: &AlertsConfig, curve_max_temp: f64) -> Self {
        Self {
            enabled: cfg.enabled,
            sustained_hot_c: cfg.sustained_hot_c,
            sustained_after_secs: cfg.sustained_after_secs,
            near_limit_temp: curve_max_temp - cfg.near_limit_margin_c,
            near_limit_margin_c: cfg.near_limit_margin_c,
            escalate_every_secs: cfg.escalate_every_secs.max(1),
            cooldown_secs: cfg.cooldown_secs,
            alert_color: tuple3(cfg.alert_color),
            fault_colors: [tuple3(cfg.fault_colors[0]), tuple3(cfg.fault_colors[1])],
            state: 0,
            hot_secs: 0,
            in_state_secs: 0,
            clear_secs: 0,
        }
    }

    fn enter_state(&mut self, state: u8) {
        self.state = state;
        self.in_state_secs = 0;
        self.clear_secs = 0;
    }

    /// elapsed = seconds since the previous tick() call.
    /// Returns Some(state-name) when the ALARM STATE changed this tick
    /// (for logging); LED output is read via led(), which the loop diffs.
    ///
    /// Unchanged signature/behavior: delegates to `tick_with` with
    /// all-absent thermal hints, so every pre-existing caller (and the 8
    /// pre-existing tests) sees exactly the legacy trigger set.
    ///
    /// `main.rs` calls `tick_with` directly (it always has real, if
    /// possibly-default, hints to pass); this stays as the documented
    /// default-hints convenience API and is exercised by the pre-existing
    /// test suite (`tick_matches_tick_with_default_hints` pins the
    /// equivalence), hence the narrow `allow(dead_code)`.
    #[allow(dead_code)]
    pub fn tick(&mut self, inputs: &AlarmInputs, elapsed: u64) -> Option<&'static str> {
        self.tick_with(inputs, &ThermalHints::default(), elapsed)
    }

    /// Same as `tick`, but also considers multi-signal thermal context: a
    /// low reported margin or active thermal throttling raises the
    /// NearLimit rung even when `control_temp`/`duty` alone would not.
    pub fn tick_with(
        &mut self,
        inputs: &AlarmInputs,
        hints: &ThermalHints,
        elapsed: u64,
    ) -> Option<&'static str> {
        if !self.enabled {
            return None;
        }

        // hot_secs tracks the SustainedHot condition independently of which
        // state is currently displayed, so a lower-priority dwell timer
        // keeps accumulating while a higher-priority state is shown.
        match inputs.control_temp {
            Some(t) if t >= self.sustained_hot_c => {
                self.hot_secs = self.hot_secs.saturating_add(elapsed);
            }
            Some(_) => self.hot_secs = 0,
            None => {} // partial sensor loss: freeze, neither accumulate nor reset
        }

        let fault_trigger = inputs.fallback_active;
        let near_limit_trigger = inputs.duty >= 100
            || inputs
                .control_temp
                .is_some_and(|t| t >= self.near_limit_temp)
            || hints
                .thermal_margin_c
                .is_some_and(|m| m <= self.near_limit_margin_c)
            || hints.thermal_throttling;
        let sustained_trigger = self.hot_secs >= self.sustained_after_secs;

        let prev = self.state;

        if fault_trigger && self.state != 3 {
            self.enter_state(3);
        } else if !fault_trigger && near_limit_trigger && self.state < 2 {
            self.enter_state(2);
        } else if !fault_trigger && !near_limit_trigger && sustained_trigger && self.state < 1 {
            self.enter_state(1);
        } else {
            // No escalation this tick: track whether the active state's own
            // trigger still holds.
            let holds = match self.state {
                3 => fault_trigger,
                2 => near_limit_trigger,
                1 => sustained_trigger,
                _ => true,
            };
            if holds {
                self.clear_secs = 0;
                self.in_state_secs = self.in_state_secs.saturating_add(elapsed);
            } else {
                self.clear_secs = self.clear_secs.saturating_add(elapsed);
                self.in_state_secs = self.in_state_secs.saturating_add(elapsed);
                if self.clear_secs >= self.cooldown_secs {
                    // Re-evaluate from the top of the states below the one
                    // that just cleared.
                    let next = if self.state > 2 && near_limit_trigger {
                        2
                    } else if self.state > 1 && sustained_trigger {
                        1
                    } else {
                        0
                    };
                    self.enter_state(next);
                }
            }
        }

        if self.state != prev {
            Some(state_name(self.state))
        } else {
            None
        }
    }

    /// None = Normal (loop uses its existing gradient/bucket logic).
    pub fn led(&self) -> Option<LedCommand> {
        match self.state {
            3 => Some(LedCommand::Runway {
                colors: self.fault_colors,
                speed_idx: 2,
            }),
            2 => {
                let steps = self.in_state_secs / self.escalate_every_secs;
                let speed_idx = std::cmp::min(4, 1 + steps) as u8;
                Some(LedCommand::Breathing {
                    color: self.alert_color,
                    speed_idx,
                })
            }
            // The loop substitutes the CURRENT gradient color for
            // alert_color here (see Task 5); the machine stays
            // self-contained and always reports cfg.alert_color.
            1 => Some(LedCommand::Breathing {
                color: self.alert_color,
                speed_idx: 0,
            }),
            _ => None,
        }
    }

    /// 0 Normal, 1 SustainedHot, 2 NearLimit, 3 Fault.
    pub fn state_code(&self) -> u8 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AlertsConfig {
        AlertsConfig {
            enabled: true,
            sustained_hot_c: 75.0,
            sustained_after_secs: 120,
            near_limit_margin_c: 3.0,
            escalate_every_secs: 30,
            cooldown_secs: 60,
            alert_color: [255, 0, 0],
            fault_colors: [[255, 80, 0], [255, 0, 0]],
        }
    }
    fn inp(t: f64, duty: u8) -> AlarmInputs {
        AlarmInputs {
            control_temp: Some(t),
            duty,
            fallback_active: false,
        }
    }

    #[test]
    fn stays_normal_below_thresholds() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        assert_eq!(m.tick(&inp(60.0, 40), 5), None);
        assert!(m.led().is_none());
        assert_eq!(m.state_code(), 0);
    }

    #[test]
    fn sustained_hot_needs_dwell_time() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        m.tick(&inp(76.0, 70), 5);
        assert_eq!(m.state_code(), 0); // hot but not yet sustained
        m.tick(&inp(76.0, 70), 119);
        assert_eq!(m.state_code(), 1); // 124s >= 120s
        assert!(matches!(
            m.led(),
            Some(LedCommand::Breathing { speed_idx: 0, .. })
        ));
    }

    #[test]
    fn near_limit_triggers_immediately_and_escalates() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        assert!(m.tick(&inp(78.0, 95), 5).is_some()); // 78 >= 80-3
        assert_eq!(m.state_code(), 2);
        assert!(matches!(
            m.led(),
            Some(LedCommand::Breathing { speed_idx: 1, .. })
        ));
        m.tick(&inp(78.5, 100), 30);
        assert!(matches!(
            m.led(),
            Some(LedCommand::Breathing { speed_idx: 2, .. })
        ));
        m.tick(&inp(79.0, 100), 60);
        assert!(matches!(
            m.led(),
            Some(LedCommand::Breathing { speed_idx: 4, .. })
        ));
        m.tick(&inp(79.0, 100), 600);
        assert!(matches!(
            m.led(),
            Some(LedCommand::Breathing { speed_idx: 4, .. })
        )); // caps
    }

    #[test]
    fn duty_pinned_triggers_near_limit_even_when_cooler() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        m.tick(&inp(70.0, 100), 5);
        assert_eq!(m.state_code(), 2);
    }

    #[test]
    fn fault_overrides_everything() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        m.tick(&inp(79.0, 100), 5);
        assert_eq!(m.state_code(), 2);
        let f = AlarmInputs {
            control_temp: None,
            duty: 60,
            fallback_active: true,
        };
        m.tick(&f, 5);
        assert_eq!(m.state_code(), 3);
        assert!(matches!(
            m.led(),
            Some(LedCommand::Runway { speed_idx: 2, .. })
        ));
    }

    #[test]
    fn deescalation_requires_cooldown() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        m.tick(&inp(78.0, 100), 5);
        assert_eq!(m.state_code(), 2);
        m.tick(&inp(60.0, 50), 30);
        assert_eq!(m.state_code(), 2); // cleared only 30s < 60s cooldown
        m.tick(&inp(60.0, 50), 35);
        assert_eq!(m.state_code(), 0); // 65s clear -> drops; sustained not held -> Normal
        assert!(m.led().is_none());
    }

    #[test]
    fn near_limit_drops_to_sustained_when_still_hot() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        m.tick(&inp(76.0, 70), 130); // SustainedHot
        assert_eq!(m.state_code(), 1);
        m.tick(&inp(79.0, 100), 5); // escalate
        assert_eq!(m.state_code(), 2);
        m.tick(&inp(76.0, 80), 65); // near-limit clear past cooldown, still >= 75
        assert_eq!(m.state_code(), 1);
    }

    #[test]
    fn none_temp_freezes_timers() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        m.tick(&inp(76.0, 70), 60);
        let gap = AlarmInputs {
            control_temp: None,
            duty: 70,
            fallback_active: false,
        };
        m.tick(&gap, 600); // sensor gap must not count as dwell
        assert_eq!(m.state_code(), 0);
        m.tick(&inp(76.0, 70), 65); // 60+65 >= 120 across the gap
        assert_eq!(m.state_code(), 1);
    }

    #[test]
    fn tick_matches_tick_with_default_hints() {
        let mut a = AlarmMachine::new(&cfg(), 80.0);
        let mut b = AlarmMachine::new(&cfg(), 80.0);
        let hints = ThermalHints::default();
        let steps: &[(f64, u8, u64)] = &[
            (60.0, 40, 5),   // Normal
            (76.0, 70, 130), // SustainedHot (dwell exceeded)
            (78.0, 100, 5),  // NearLimit (escalates immediately)
            (60.0, 50, 30),  // clearing, within cooldown
            (60.0, 50, 35),  // cooldown exceeded -> de-escalates
        ];
        for (temp, duty, elapsed) in steps.iter().copied() {
            let inputs = inp(temp, duty);
            let ra = a.tick(&inputs, elapsed);
            let rb = b.tick_with(&inputs, &hints, elapsed);
            assert_eq!(ra, rb);
            assert_eq!(a.state_code(), b.state_code());
        }
    }

    #[test]
    fn low_thermal_margin_triggers_near_limit() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        let hints = ThermalHints {
            thermal_margin_c: Some(2.0), // <= near_limit_margin_c (3.0)
            thermal_throttling: false,
        };
        m.tick_with(&inp(40.0, 50), &hints, 5);
        assert_eq!(m.state_code(), 2);
    }

    #[test]
    fn thermal_throttling_triggers_near_limit_when_cool() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        let hints = ThermalHints {
            thermal_margin_c: None,
            thermal_throttling: true,
        };
        m.tick_with(&inp(60.0, 40), &hints, 5);
        assert_eq!(m.state_code(), 2); // proves independence from temp/duty
    }

    #[test]
    fn absent_margin_does_not_trigger_near_limit() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        let hints = ThermalHints {
            thermal_margin_c: None,
            thermal_throttling: false,
        };
        m.tick_with(&inp(40.0, 40), &hints, 5);
        assert_eq!(m.state_code(), 0);
    }

    #[test]
    fn margin_recovery_deescalates_after_cooldown() {
        let mut m = AlarmMachine::new(&cfg(), 80.0);
        let low = ThermalHints {
            thermal_margin_c: Some(2.0),
            thermal_throttling: false,
        };
        m.tick_with(&inp(60.0, 50), &low, 5);
        assert_eq!(m.state_code(), 2);

        let recovered = ThermalHints {
            thermal_margin_c: Some(25.0), // well above near_limit_margin_c
            thermal_throttling: false,
        };
        m.tick_with(&inp(60.0, 50), &recovered, 30);
        assert_eq!(m.state_code(), 2); // cleared only 30s < 60s cooldown

        m.tick_with(&inp(60.0, 50), &recovered, 35);
        assert_eq!(m.state_code(), 0); // 65s clear -> drops (not sustained)
    }
}
