//! Pure multi-signal fusion core.
//!
//! Nothing in this module touches NVML or any other hardware API — it is
//! deliberately kept import-free of `nvml_wrapper` so the whole fusion
//! pipeline (plausibility filtering, asymmetric smoothing, curve lookup,
//! max-of-candidates fusion, throttle floor) is exercisable and testable
//! without a GPU. Wave 3 (`src/sensors.rs`) is responsible for producing
//! `SignalReadings` from real hardware; this module only consumes them.
//!
//! Not yet wired into the control loop (Wave 7 does that), so `#[allow(dead_code)]`
//! suppresses the expected "never constructed/used" lint for this wave.
#![allow(dead_code)]

use crate::config::{CurvePoint, PowerUnit, SignalsConfig, ThrottleReason};

/// One of the five signals the fusion core can fold into a duty target.
///
/// Declaration order IS tie-break precedence (earlier wins ties in `fuse`)
/// — hard limits (memory temp, thermal margin) are listed first so they win
/// over softer signals (GPU power, CPU temp) when candidate duties are
/// exactly equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SignalKind {
    MemTemp,
    ThermalMargin,
    GpuTemp,
    GpuPower,
    CpuTemp,
}

impl SignalKind {
    pub const ALL: [SignalKind; 5] = [
        SignalKind::MemTemp,
        SignalKind::ThermalMargin,
        SignalKind::GpuTemp,
        SignalKind::GpuPower,
        SignalKind::CpuTemp,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            SignalKind::MemTemp => "mem_temp",
            SignalKind::ThermalMargin => "thermal_margin",
            SignalKind::GpuTemp => "gpu_temp",
            SignalKind::GpuPower => "gpu_power",
            SignalKind::CpuTemp => "cpu_temp",
        }
    }

    pub fn unit(&self) -> &'static str {
        match self {
            SignalKind::GpuPower => "watts",
            _ => "celsius",
        }
    }

    /// Index into the 5-element per-signal arrays used throughout this
    /// module (`SignalConditioner`'s filters, `Conditioned::values`,
    /// `Fusion::candidates`). Matches declaration order.
    pub fn idx(&self) -> usize {
        *self as usize
    }
}

/// Raw (unfiltered) readings for one tick. All fields `Option` because
/// "absent" (sensor not supported / not yet probed) is a distinct, common
/// state from any real value — including zero.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SignalReadings {
    pub gpu_temp_c: Option<f64>,
    pub cpu_temp_c: Option<f64>,
    pub gpu_power_w: Option<f64>,
    pub gpu_power_limit_w: Option<f64>,
    pub thermal_margin_c: Option<f64>,
    pub mem_temp_c: Option<f64>,
    pub throttle: ThrottleFlags,
}

/// Snapshot of NVML throttle-reason bits relevant to fan control.
/// `sw_power_cap` is tracked but deliberately excluded from throttle-floor
/// defaults — Wave 0 hardware data shows it asserts continuously under any
/// sustained load on this card, so including it in the floor trigger set
/// would pin fans at the floor duty for the entire duration of every job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThrottleFlags {
    pub sw_thermal: bool,
    pub hw_thermal: bool,
    pub hw_power_brake: bool,
    pub sw_power_cap: bool,
}

impl ThrottleFlags {
    pub fn any_of(&self, reasons: &[ThrottleReason]) -> bool {
        reasons.iter().any(|r| match r {
            ThrottleReason::SwThermal => self.sw_thermal,
            ThrottleReason::HwThermal => self.hw_thermal,
            ThrottleReason::HwPowerBrake => self.hw_power_brake,
            ThrottleReason::SwPowerCap => self.sw_power_cap,
        })
    }
}

/// Plausibility guard: rejects non-finite values and values outside a
/// physically sane range for the signal's kind.
///
/// RATIONALE: a dead signal may return `Ok(0)` rather than `NotSupported` —
/// DCGM reported memory temp as a constant 0 for 6875 samples on this
/// hardware — so error-based absence detection is insufficient on its own.
/// (Wave 0 on this specific card found NVML's memory-temp field *does* fail
/// cleanly with `NotSupported`, so the error path is the primary defense
/// here — but the plausibility guard stays as defense-in-depth for other
/// drivers/cards that don't fail as cleanly, and as the last line of
/// defense against any signal source that returns a sentinel value.)
pub fn plausible(kind: SignalKind, v: f64) -> bool {
    if !v.is_finite() {
        return false;
    }
    match kind {
        SignalKind::MemTemp | SignalKind::GpuTemp | SignalKind::CpuTemp => {
            (1.0..=125.0).contains(&v)
        }
        SignalKind::GpuPower => (1.0..=2000.0).contains(&v),
        SignalKind::ThermalMargin => (-40.0..=200.0).contains(&v),
    }
}

/// Exponential weighted moving average: `alpha * sample + (1 - alpha) * prev`.
pub fn ewma(prev: f64, sample: f64, alpha: f64) -> f64 {
    alpha * sample + (1.0 - alpha) * prev
}

/// An EWMA filter with different smoothing constants for rising vs. falling
/// samples.
///
/// WHY: on the falling edge of a load release, GPU power shed ~96% of its
/// range in ~20 s while die temperature had shed only about half its range
/// and kept decaying for 40+ seconds more (Wave 0 hardware data, RTX 5090).
/// A symmetric filter forces a bad compromise: fast enough to track power's
/// leading edge means it also drops the temperature-derived candidates
/// (and therefore fan duty) far too quickly while heat is still in the
/// heatsink, and slow enough to track temperature's decay means it blunts
/// power's whole value as a *leading* indicator on the rising edge. Rising
/// fast / falling slow keeps both properties: quick to respond to a load
/// step, cautious about declaring the danger over.
#[derive(Debug, Clone, Copy)]
pub struct AsymEwma {
    value: Option<f64>,
    rise_alpha: f64,
    fall_alpha: f64,
}

impl AsymEwma {
    pub fn new(rise_alpha: f64, fall_alpha: f64) -> Self {
        Self {
            value: None,
            rise_alpha,
            fall_alpha,
        }
    }

    pub fn symmetric(alpha: f64) -> Self {
        Self::new(alpha, alpha)
    }

    /// Feeds one sample. `None` HOLDS internal state and returns `None` — a
    /// one-tick NVML hiccup must not reset the filter, but it also does not
    /// manufacture a fresh reading for this tick. The first `Some` sample
    /// seeds `value` directly (no cold-start ramp from zero); subsequent
    /// samples blend with `rise_alpha` when the sample is above the current
    /// value, `fall_alpha` otherwise.
    pub fn update(&mut self, sample: Option<f64>) -> Option<f64> {
        let s = sample?;
        let next = match self.value {
            None => s,
            Some(prev) => {
                let alpha = if s > prev {
                    self.rise_alpha
                } else {
                    self.fall_alpha
                };
                ewma(prev, s, alpha)
            }
        };
        self.value = Some(next);
        Some(next)
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn reset(&mut self) {
        self.value = None;
    }
}

/// Output of `SignalConditioner::update`: plausibility-filtered,
/// asymmetrically-smoothed values for all 5 signals, plus the passthrough
/// state (`gpu_power_limit_w`, `throttle`) that fusion needs but that isn't
/// itself smoothed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Conditioned {
    pub values: [Option<f64>; 5],
    pub gpu_power_limit_w: Option<f64>,
    pub throttle: ThrottleFlags,
}

/// Applies plausibility filtering then per-signal asymmetric-EWMA smoothing
/// to raw readings, one filter per `SignalKind`.
pub struct SignalConditioner {
    filters: [AsymEwma; 5],
}

impl SignalConditioner {
    /// Builds one filter per signal from that signal's own config: a
    /// symmetric `alpha` for the four temperature/margin signals, and the
    /// asymmetric `rise_alpha`/`fall_alpha` pair for GPU power (the one
    /// signal that needs to rise fast and fall slow — see `AsymEwma`'s
    /// doc comment). Indexed via `kind.idx()` per assignment so the filter
    /// array's order can never drift from `SignalKind`'s declaration order.
    pub fn new(cfg: &SignalsConfig) -> Self {
        let mut filters = [AsymEwma::symmetric(1.0); 5];
        for &kind in SignalKind::ALL.iter() {
            filters[kind.idx()] = match kind {
                SignalKind::MemTemp => AsymEwma::symmetric(cfg.mem_temp.alpha),
                SignalKind::ThermalMargin => AsymEwma::symmetric(cfg.thermal_margin.alpha),
                SignalKind::GpuTemp => AsymEwma::symmetric(cfg.gpu_temp.alpha),
                SignalKind::GpuPower => {
                    AsymEwma::new(cfg.gpu_power.rise_alpha, cfg.gpu_power.fall_alpha)
                }
                SignalKind::CpuTemp => AsymEwma::symmetric(cfg.cpu_temp.alpha),
            };
        }
        Self { filters }
    }

    /// Applies `plausible()` THEN the filter. An implausible sample is
    /// dropped exactly like an absent one (fed to the filter as `None`) —
    /// it must not poison filter state (e.g. a spurious 0 must not drag
    /// a smoothed value toward zero).
    pub fn update(&mut self, raw: &SignalReadings) -> Conditioned {
        let mut values = [None; 5];
        for &kind in SignalKind::ALL.iter() {
            let raw_v = match kind {
                SignalKind::MemTemp => raw.mem_temp_c,
                SignalKind::ThermalMargin => raw.thermal_margin_c,
                SignalKind::GpuTemp => raw.gpu_temp_c,
                SignalKind::GpuPower => raw.gpu_power_w,
                SignalKind::CpuTemp => raw.cpu_temp_c,
            };
            let sample = raw_v.filter(|&v| plausible(kind, v));
            values[kind.idx()] = self.filters[kind.idx()].update(sample);
        }
        Conditioned {
            values,
            gpu_power_limit_w: raw.gpu_power_limit_w,
            throttle: raw.throttle,
        }
    }

    /// Resets GPU-derived filters only (MemTemp, ThermalMargin, GpuTemp,
    /// GpuPower) — used on GPU re-discovery, where a fresh NVML handle
    /// means old filter state no longer corresponds to a live device.
    /// CpuTemp is untouched: the CPU sensor isn't affected by GPU
    /// re-discovery.
    pub fn reset_gpu(&mut self) {
        for kind in [
            SignalKind::MemTemp,
            SignalKind::ThermalMargin,
            SignalKind::GpuTemp,
            SignalKind::GpuPower,
        ] {
            self.filters[kind.idx()].reset();
        }
    }
}

/// Anti-flap latch: throttle-reason bits can toggle sub-second, but the
/// control loop samples at ~0.2 Hz (5 s tick), so a bit that was active at
/// any point within the last `hold_secs` should still read as active.
pub struct ThrottleLatch {
    remaining_secs: u64,
    hold_secs: u64,
}

impl ThrottleLatch {
    pub fn new(hold_secs: u64) -> Self {
        Self {
            remaining_secs: 0,
            hold_secs,
        }
    }

    /// Advances the latch by `elapsed_secs`. Returns true while `active` is
    /// true, or while still within `hold_secs` of the last time it was.
    pub fn update(&mut self, active: bool, elapsed_secs: u64) -> bool {
        if active {
            self.remaining_secs = self.hold_secs;
            true
        } else {
            self.remaining_secs = self.remaining_secs.saturating_sub(elapsed_secs);
            self.remaining_secs > 0
        }
    }

    pub fn active(&self) -> bool {
        self.remaining_secs > 0
    }
}

/// One signal's contribution to fusion: its (conditioned) value and the
/// duty its curve maps that value to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    pub kind: SignalKind,
    pub value: f64,
    pub duty: u8,
}

/// Result of `fuse`: every signal's candidate (if it produced one), the
/// fused target duty, which signal won, and whether the throttle floor
/// changed the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Fusion {
    pub candidates: [Option<Candidate>; 5],
    pub target: Option<u8>,
    pub winner: Option<SignalKind>,
    pub floor_applied: bool,
}

/// Folds conditioned readings into a single duty target.
///
/// For each enabled signal with a `Some` conditioned value: pick its
/// per-signal curve if configured (non-empty), else fall back to
/// `top_curve` — but only for `gpu_temp`/`cpu_temp`; `thermal_margin`,
/// `gpu_power`, and `mem_temp` require their own curve (enforced by config
/// validation starting Wave 2) and are skipped here if it's empty, rather
/// than silently reinterpreting `top_curve`'s temperature axis as theirs.
///
/// `gpu_power` in `percent_tdp` mode with no power limit available is
/// SKIPPED entirely — never silently reinterpreted as watts.
///
/// The fused `target` is the max candidate duty (raise-only fusion), ties
/// broken by `SignalKind` declaration order. If `throttle_latched` and
/// `cfg.throttle.enabled`, the target is raised to at least
/// `cfg.throttle.floor_duty` — this can produce `Some(target)` even when
/// every curve signal is absent, which is deliberate and safe (the floor
/// is a hardware safety net, not dependent on any one signal being
/// available). `winner` names the candidate that set the (pre-floor) max,
/// or `None` when only the floor produced a target.
///
/// CONTRACT: `fuse` trusts `cond.values` to already be plausibility-filtered
/// and finite — that's `SignalConditioner`'s job, not this function's. A
/// `NaN` (or other non-finite value) injected directly into `Conditioned`,
/// bypassing `SignalConditioner`, would fall through `interpolate`'s
/// less-than/greater-than-or-equal comparisons (all `false` against `NaN`)
/// to the loop body, then to the final `last.duty` fallback — silently
/// producing a plausible-looking duty instead of an error. Relevant to
/// Wave 7 callers: always route readings through `SignalConditioner::update`
/// before calling `fuse`, never construct `Conditioned` by hand from raw
/// sensor output.
pub fn fuse(
    cfg: &SignalsConfig,
    top_curve: &[CurvePoint],
    cond: &Conditioned,
    throttle_latched: bool,
) -> Fusion {
    let mut candidates: [Option<Candidate>; 5] = Default::default();

    for &kind in SignalKind::ALL.iter() {
        let Some(raw_value) = cond.values[kind.idx()] else {
            continue;
        };

        let (enabled, own_curve): (bool, &[CurvePoint]) = match kind {
            SignalKind::MemTemp => (cfg.mem_temp.enabled, &cfg.mem_temp.curve),
            SignalKind::ThermalMargin => (cfg.thermal_margin.enabled, &cfg.thermal_margin.curve),
            SignalKind::GpuTemp => (cfg.gpu_temp.enabled, &cfg.gpu_temp.curve),
            SignalKind::GpuPower => (cfg.gpu_power.enabled, &cfg.gpu_power.curve),
            SignalKind::CpuTemp => (cfg.cpu_temp.enabled, &cfg.cpu_temp.curve),
        };
        if !enabled {
            continue;
        }

        let resolved: Option<(f64, &[CurvePoint])> = match kind {
            SignalKind::CpuTemp => {
                let x = raw_value - cfg.cpu_temp.offset_c;
                let curve = if !own_curve.is_empty() {
                    own_curve
                } else {
                    top_curve
                };
                Some((x, curve))
            }
            SignalKind::GpuTemp => {
                let curve = if !own_curve.is_empty() {
                    own_curve
                } else {
                    top_curve
                };
                Some((raw_value, curve))
            }
            SignalKind::GpuPower => {
                if own_curve.is_empty() {
                    None
                } else {
                    match cfg.gpu_power.unit {
                        PowerUnit::Watts => Some((raw_value, own_curve)),
                        PowerUnit::PercentTdp => match cond.gpu_power_limit_w {
                            Some(limit) if limit > 0.0 => {
                                Some((100.0 * raw_value / limit, own_curve))
                            }
                            _ => None,
                        },
                    }
                }
            }
            SignalKind::ThermalMargin | SignalKind::MemTemp => {
                if own_curve.is_empty() {
                    None
                } else {
                    Some((raw_value, own_curve))
                }
            }
        };

        let Some((x, curve)) = resolved else {
            continue;
        };
        if curve.is_empty() {
            continue;
        }
        let duty = crate::curve::interpolate(curve, x);
        candidates[kind.idx()] = Some(Candidate {
            kind,
            value: raw_value,
            duty,
        });
    }

    let mut best: Option<Candidate> = None;
    for c in candidates.iter().flatten() {
        best = match best {
            None => Some(*c),
            Some(b) if c.duty > b.duty => Some(*c),
            Some(b) => Some(b), // earlier (declaration-order) candidate keeps ties
        };
    }

    let mut target = best.map(|c| c.duty);
    let winner = best.map(|c| c.kind);
    let mut floor_applied = false;

    if throttle_latched && cfg.throttle.enabled {
        let floor = cfg.throttle.floor_duty;
        let baseline = target.unwrap_or(0);
        let raised = baseline.max(floor);
        floor_applied = raised > baseline;
        target = Some(raised);
    }

    Fusion {
        candidates,
        target,
        winner,
        floor_applied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_curve() -> Vec<CurvePoint> {
        vec![
            CurvePoint { temp: 0.0, duty: 0 },
            CurvePoint {
                temp: 100.0,
                duty: 100,
            },
        ]
    }

    // -- ewma / AsymEwma -------------------------------------------------

    #[test]
    fn ewma_is_weighted_average() {
        assert_eq!(ewma(10.0, 20.0, 0.25), 12.5);
    }

    #[test]
    fn asym_ewma_rises_faster_than_it_falls() {
        // Real Wave-0 numbers: idle ~17 W, load-peak ~579 W, post-release ~21 W.
        let mut f = AsymEwma::new(0.5, 0.1);
        f.update(Some(17.0)); // seed
        let risen = f.update(Some(579.0)).unwrap();
        assert_eq!(risen, 0.5 * 579.0 + 0.5 * 17.0);

        let fallen = f.update(Some(21.0)).unwrap();
        assert!((fallen - (0.1 * 21.0 + 0.9 * risen)).abs() < 1e-9);

        let rise_frac = (risen - 17.0) / (579.0 - 17.0);
        let fall_frac = (risen - fallen) / (risen - 21.0);
        assert!(
            rise_frac > fall_frac,
            "rise_frac={rise_frac} should exceed fall_frac={fall_frac}"
        );
    }

    #[test]
    fn asym_ewma_first_sample_seeds_directly() {
        let mut f = AsymEwma::new(0.5, 0.1);
        assert_eq!(f.update(Some(42.0)), Some(42.0));
        assert_eq!(f.value(), Some(42.0));
    }

    #[test]
    fn asym_ewma_missing_sample_holds_state() {
        let mut f = AsymEwma::new(0.5, 0.1);
        f.update(Some(50.0));
        assert_eq!(f.update(None), None);
        assert_eq!(f.value(), Some(50.0)); // held, not reset

        // Continues from the held state, not a cold-start reset.
        let next = f.update(Some(60.0)).unwrap();
        assert_eq!(next, 0.5 * 60.0 + 0.5 * 50.0);
    }

    #[test]
    fn asym_ewma_reset_clears_state() {
        let mut f = AsymEwma::new(0.5, 0.1);
        f.update(Some(50.0));
        f.reset();
        assert_eq!(f.value(), None);
        // Next sample seeds directly again rather than blending with stale state.
        assert_eq!(f.update(Some(10.0)), Some(10.0));
    }

    // -- plausible ---------------------------------------------------------

    #[test]
    fn plausible_rejects_zero_memory_temp() {
        assert!(!plausible(SignalKind::MemTemp, 0.0));
    }

    #[test]
    fn plausible_rejects_non_finite() {
        assert!(!plausible(SignalKind::GpuTemp, f64::NAN));
        assert!(!plausible(SignalKind::GpuTemp, f64::INFINITY));
        assert!(!plausible(SignalKind::GpuPower, f64::NEG_INFINITY));
    }

    #[test]
    fn plausible_allows_negative_margin() {
        assert!(plausible(SignalKind::ThermalMargin, -10.0));
        assert!(!plausible(SignalKind::ThermalMargin, -41.0));
    }

    // -- SignalConditioner ---------------------------------------------------

    #[test]
    fn conditioner_drops_implausible_sample_without_poisoning_filter() {
        let mut cfg = SignalsConfig::default();
        cfg.mem_temp.alpha = 0.5; // exercise blending explicitly rather than the default passthrough (alpha 1.0)
        let mut cond = SignalConditioner::new(&cfg);

        // Prime with a plausible reading; first sample seeds the filter directly.
        let primed = cond.update(&SignalReadings {
            mem_temp_c: Some(40.0),
            ..Default::default()
        });
        assert_eq!(primed.values[SignalKind::MemTemp.idx()], Some(40.0));

        // The real DCGM failure mode: memory temp reported as a constant 0
        // (Ok(0), not an error). plausible() must reject it, and the
        // conditioner must feed the filter `None`, not the bad 0.0 sample —
        // the conditioned value must stay exactly at the primed value, not
        // be dragged toward zero.
        let after_bad_sample = cond.update(&SignalReadings {
            mem_temp_c: Some(0.0),
            ..Default::default()
        });
        assert_eq!(
            after_bad_sample.values[SignalKind::MemTemp.idx()],
            None,
            "an implausible sample must not produce a fresh conditioned reading"
        );

        // Filter state itself must be untouched by the implausible sample:
        // the next *plausible* sample must blend from the primed value
        // (40.0), not from a poisoned/zeroed state.
        let after_good_sample = cond.update(&SignalReadings {
            mem_temp_c: Some(50.0), // rise: 0.5*50 + 0.5*40 = 45.0
            ..Default::default()
        });
        assert_eq!(
            after_good_sample.values[SignalKind::MemTemp.idx()],
            Some(45.0)
        );
    }

    #[test]
    fn conditioner_reset_gpu_preserves_cpu_filter() {
        let mut cfg = SignalsConfig::default();
        cfg.cpu_temp.alpha = 0.5; // exercise blending explicitly rather than the default passthrough (alpha 1.0)
        let mut cond = SignalConditioner::new(&cfg);

        // Prime all five filters (first sample seeds each directly).
        cond.update(&SignalReadings {
            mem_temp_c: Some(40.0),
            thermal_margin_c: Some(50.0),
            gpu_temp_c: Some(60.0),
            gpu_power_w: Some(100.0),
            cpu_temp_c: Some(70.0),
            ..Default::default()
        });

        cond.reset_gpu();

        // Feed a second, distinct sample to every signal. The four
        // GPU-derived filters were reset, so they must seed directly on
        // this new sample (conditioned value == raw sample). CpuTemp was
        // NOT reset, so it must blend from its held state instead.
        let after_reset = cond.update(&SignalReadings {
            mem_temp_c: Some(42.0),
            thermal_margin_c: Some(52.0),
            gpu_temp_c: Some(62.0),
            gpu_power_w: Some(105.0),
            cpu_temp_c: Some(75.0), // rise: 0.5*75 + 0.5*70 = 72.5
            ..Default::default()
        });

        assert_eq!(after_reset.values[SignalKind::MemTemp.idx()], Some(42.0));
        assert_eq!(
            after_reset.values[SignalKind::ThermalMargin.idx()],
            Some(52.0)
        );
        assert_eq!(after_reset.values[SignalKind::GpuTemp.idx()], Some(62.0));
        assert_eq!(after_reset.values[SignalKind::GpuPower.idx()], Some(105.0));
        assert_eq!(
            after_reset.values[SignalKind::CpuTemp.idx()],
            Some(72.5),
            "CpuTemp filter must survive reset_gpu() and keep blending from held state"
        );
    }

    #[test]
    fn conditioner_uses_per_signal_alphas() {
        let mut cfg = SignalsConfig::default();
        cfg.gpu_temp.alpha = 1.0; // passthrough: tracks the raw sample exactly
        cfg.thermal_margin.alpha = 0.5; // symmetric blend, halfway either direction
        cfg.gpu_power.rise_alpha = 0.5;
        cfg.gpu_power.fall_alpha = 0.1; // rises fast, falls slow
        let mut cond = SignalConditioner::new(&cfg);

        // Prime all three filters at the same starting value.
        let primed = cond.update(&SignalReadings {
            gpu_temp_c: Some(40.0),
            thermal_margin_c: Some(40.0),
            gpu_power_w: Some(40.0),
            ..Default::default()
        });
        assert_eq!(primed.values[SignalKind::GpuTemp.idx()], Some(40.0));
        assert_eq!(primed.values[SignalKind::ThermalMargin.idx()], Some(40.0));
        assert_eq!(primed.values[SignalKind::GpuPower.idx()], Some(40.0));

        // Rising edge, same-sized step (40 -> 80) fed to all three:
        // gpu_temp (alpha 1.0) jumps straight to the raw sample; margin
        // (alpha 0.5, symmetric) lands exactly halfway; power's rise_alpha
        // is also 0.5, so it lands at the same halfway point on the rise.
        let risen = cond.update(&SignalReadings {
            gpu_temp_c: Some(80.0),
            thermal_margin_c: Some(80.0),
            gpu_power_w: Some(80.0),
            ..Default::default()
        });
        assert_eq!(
            risen.values[SignalKind::GpuTemp.idx()],
            Some(80.0),
            "gpu_temp alpha=1.0 must track the raw sample exactly"
        );
        assert_eq!(
            risen.values[SignalKind::ThermalMargin.idx()],
            Some(60.0),
            "margin alpha=0.5 must land halfway between old and new"
        );
        assert_eq!(
            risen.values[SignalKind::GpuPower.idx()],
            Some(60.0),
            "power rise_alpha=0.5 must land halfway on the rising edge"
        );

        // Falling edge, same-sized step (80 -> 20) fed to all three:
        // gpu_temp again tracks exactly (alpha 1.0, symmetric); margin again
        // lands halfway (alpha 0.5, symmetric: 0.5*20 + 0.5*60 = 40); power's
        // fall_alpha=0.1 must move far less (0.1*20 + 0.9*60 = 56) than
        // margin's symmetric 0.5 on an identical step — proving power's
        // rise/fall smoothing is genuinely asymmetric, not just reusing one
        // shared config knob.
        let fallen = cond.update(&SignalReadings {
            gpu_temp_c: Some(20.0),
            thermal_margin_c: Some(20.0),
            gpu_power_w: Some(20.0),
            ..Default::default()
        });
        assert_eq!(fallen.values[SignalKind::GpuTemp.idx()], Some(20.0));
        assert_eq!(fallen.values[SignalKind::ThermalMargin.idx()], Some(40.0));
        assert_eq!(fallen.values[SignalKind::GpuPower.idx()], Some(56.0));

        let margin_fall = risen.values[SignalKind::ThermalMargin.idx()].unwrap()
            - fallen.values[SignalKind::ThermalMargin.idx()].unwrap();
        let power_fall = risen.values[SignalKind::GpuPower.idx()].unwrap()
            - fallen.values[SignalKind::GpuPower.idx()].unwrap();
        assert!(
            power_fall < margin_fall,
            "power (fall_alpha=0.1) must fall less than margin (alpha=0.5) on an identical step: power_fall={power_fall} margin_fall={margin_fall}"
        );
    }

    // -- fuse ----------------------------------------------------------------

    #[test]
    fn fuse_takes_max_of_candidate_duties() {
        let mut cfg = SignalsConfig::default();
        cfg.gpu_temp.enabled = true; // curve empty -> falls back to top_curve
        cfg.cpu_temp.enabled = true;
        cfg.cpu_temp.curve = linear_curve();
        cfg.cpu_temp.offset_c = 0.0; // isolate max-of-candidates from offset behavior (covered separately below)

        let mut cond = Conditioned::default();
        cond.values[SignalKind::GpuTemp.idx()] = Some(40.0);
        cond.values[SignalKind::CpuTemp.idx()] = Some(70.0);

        let fusion = fuse(&cfg, &linear_curve(), &cond, false);
        assert_eq!(fusion.target, Some(70));
        assert_eq!(fusion.winner, Some(SignalKind::CpuTemp));
    }

    #[test]
    fn fuse_skips_absent_signals() {
        let mut cfg = SignalsConfig::default();
        cfg.gpu_temp.enabled = true;
        cfg.cpu_temp.enabled = true;
        cfg.cpu_temp.curve = linear_curve();

        let mut cond = Conditioned::default();
        cond.values[SignalKind::GpuTemp.idx()] = Some(40.0);
        // cpu_temp left absent (None)

        let fusion = fuse(&cfg, &linear_curve(), &cond, false);
        assert!(fusion.candidates[SignalKind::CpuTemp.idx()].is_none());
        assert_eq!(fusion.target, Some(40));
        assert_eq!(fusion.winner, Some(SignalKind::GpuTemp));
    }

    #[test]
    fn fuse_returns_none_when_no_signal_available() {
        let cfg = SignalsConfig::default(); // nothing enabled
        let cond = Conditioned::default();
        let fusion = fuse(&cfg, &linear_curve(), &cond, false);
        assert_eq!(fusion.target, None);
        assert_eq!(fusion.winner, None);
        assert!(!fusion.floor_applied);
    }

    #[test]
    fn fuse_ties_break_by_declared_precedence() {
        let mut cfg = SignalsConfig::default();
        cfg.mem_temp.enabled = true;
        cfg.mem_temp.curve = linear_curve();
        cfg.gpu_temp.enabled = true; // falls back to top_curve

        let mut cond = Conditioned::default();
        cond.values[SignalKind::MemTemp.idx()] = Some(50.0); // duty 50
        cond.values[SignalKind::GpuTemp.idx()] = Some(50.0); // duty 50, tie

        let fusion = fuse(&cfg, &linear_curve(), &cond, false);
        assert_eq!(fusion.target, Some(50));
        // MemTemp is declared before GpuTemp, so it wins the tie.
        assert_eq!(fusion.winner, Some(SignalKind::MemTemp));
    }

    #[test]
    fn fuse_applies_cpu_offset() {
        let mut cfg = SignalsConfig::default();
        cfg.cpu_temp.enabled = true;
        cfg.cpu_temp.offset_c = 10.0;
        // curve left empty -> falls back to top_curve

        let mut cond = Conditioned::default();
        cond.values[SignalKind::CpuTemp.idx()] = Some(45.0); // x = 45 - 10 = 35

        let fusion = fuse(&cfg, &linear_curve(), &cond, false);
        assert_eq!(fusion.target, Some(35));
        assert_eq!(fusion.winner, Some(SignalKind::CpuTemp));
    }

    #[test]
    fn fuse_margin_curve_is_inverted() {
        let mut cfg = SignalsConfig::default();
        cfg.thermal_margin.enabled = true;
        cfg.thermal_margin.curve = vec![
            CurvePoint {
                temp: 5.0,
                duty: 100,
            },
            CurvePoint {
                temp: 10.0,
                duty: 85,
            },
            CurvePoint {
                temp: 20.0,
                duty: 60,
            },
            CurvePoint {
                temp: 35.0,
                duty: 30,
            },
        ];

        let mut cond = Conditioned::default();
        cond.values[SignalKind::ThermalMargin.idx()] = Some(5.0); // hot: little headroom
        let hot = fuse(&cfg, &linear_curve(), &cond, false);
        assert_eq!(hot.target, Some(100));

        cond.values[SignalKind::ThermalMargin.idx()] = Some(35.0); // cool: lots of headroom
        let cool = fuse(&cfg, &linear_curve(), &cond, false);
        assert_eq!(cool.target, Some(30));

        assert!(hot.target.unwrap() > cool.target.unwrap());
    }

    #[test]
    fn fuse_power_percent_tdp_scales_by_limit() {
        let mut cfg = SignalsConfig::default();
        cfg.gpu_power.enabled = true;
        cfg.gpu_power.unit = PowerUnit::PercentTdp;
        cfg.gpu_power.curve = linear_curve(); // duty == percent 1:1

        let mut cond = Conditioned::default();
        cond.values[SignalKind::GpuPower.idx()] = Some(300.0);
        cond.gpu_power_limit_w = Some(575.0);

        let fusion = fuse(&cfg, &linear_curve(), &cond, false);
        let expected_pct: f64 = 100.0 * 300.0 / 575.0;
        assert_eq!(fusion.target, Some(expected_pct.round() as u8));

        // limit unavailable -> power signal skipped entirely, never
        // reinterpreted as watts.
        cond.gpu_power_limit_w = None;
        let fusion2 = fuse(&cfg, &linear_curve(), &cond, false);
        assert_eq!(fusion2.target, None);
        assert!(fusion2.candidates[SignalKind::GpuPower.idx()].is_none());

        // limit == 0.0 must also skip, not divide-by-zero into an infinite
        // or NaN percentage.
        cond.gpu_power_limit_w = Some(0.0);
        let fusion3 = fuse(&cfg, &linear_curve(), &cond, false);
        assert_eq!(fusion3.target, None);
        assert!(fusion3.candidates[SignalKind::GpuPower.idx()].is_none());

        // A negative limit is nonsensical hardware state; must also skip
        // rather than reinterpret as watts or produce a negative percentage.
        cond.gpu_power_limit_w = Some(-10.0);
        let fusion4 = fuse(&cfg, &linear_curve(), &cond, false);
        assert_eq!(fusion4.target, None);
        assert!(fusion4.candidates[SignalKind::GpuPower.idx()].is_none());
    }

    #[test]
    fn fuse_throttle_floor_raises_target_and_sets_flag() {
        let mut cfg = SignalsConfig::default();
        cfg.throttle.enabled = true;
        cfg.throttle.floor_duty = 80;

        let cond = Conditioned::default(); // no curve signals at all
        let fusion = fuse(&cfg, &linear_curve(), &cond, true);
        assert_eq!(fusion.target, Some(80));
        assert!(fusion.floor_applied);
        assert_eq!(fusion.winner, None); // floor alone set it

        // Floor below an existing candidate must not lower it, and must not
        // be reported as having applied (raise-only fusion).
        let mut cfg2 = cfg.clone();
        cfg2.throttle.floor_duty = 10;
        cfg2.gpu_temp.enabled = true;
        let mut cond2 = Conditioned::default();
        cond2.values[SignalKind::GpuTemp.idx()] = Some(90.0); // duty 90 via top_curve

        let fusion2 = fuse(&cfg2, &linear_curve(), &cond2, true);
        assert_eq!(fusion2.target, Some(90));
        assert!(!fusion2.floor_applied);
        assert_eq!(fusion2.winner, Some(SignalKind::GpuTemp));

        // Floor exactly EQUAL to the candidate max must not count as having
        // "raised" it — max(90, 90) == 90 is not a strict raise.
        let mut cfg3 = cfg.clone();
        cfg3.throttle.floor_duty = 90;
        cfg3.gpu_temp.enabled = true;
        let mut cond3 = Conditioned::default();
        cond3.values[SignalKind::GpuTemp.idx()] = Some(90.0); // duty 90 via top_curve

        let fusion3 = fuse(&cfg3, &linear_curve(), &cond3, true);
        assert_eq!(fusion3.target, Some(90));
        assert!(!fusion3.floor_applied);
        assert_eq!(fusion3.winner, Some(SignalKind::GpuTemp));
    }

    // -- ThrottleLatch -----------------------------------------------------

    #[test]
    fn throttle_latch_holds_after_clear() {
        let mut latch = ThrottleLatch::new(10);
        assert!(latch.update(true, 1));
        assert!(latch.update(false, 5)); // 5s remain of the 10s hold
        assert!(!latch.update(false, 10)); // hold expired
    }
}
