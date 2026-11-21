//! Fan self-test state machine (pure).
//!
//! Owns all self-test timing: the staleness clock for settled full-tilt
//! RPM observations, the sweep's duration, the settle time, and the retry
//! backoff. The tick loop consults it at four points per tick: before the
//! decision block (`begin_tick`), after the decision block (`plan`), at
//! the duty write and alarm input (`write_duty`/`alarm_duty`), and after
//! the RPM reads (`observe`). Passive observation always runs, regardless
//! of `SelftestConfig::enabled`; only the sweep is opt-in.

use crate::config::{SelftestConfig, SELFTEST_SETTLE_SECS};

/// The constant duty a sweep drives the fans to.
pub const FULL_DUTY: u8 = 100;
/// Backoff after a sweep that produced no settled observation.
pub const RETRY_BACKOFF_SECS: u64 = 3600;

/// What the tick loop should do about the sweep this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// No sweep, none starting this tick.
    Idle,
    /// Start a sweep this tick (the hub duty is about to be rewritten 100).
    Start,
    /// Keep holding the sweep at 100.
    Hold,
    /// An active sweep was aborted (sensor-loss fallback began); the normal
    /// fallback duty write proceeds.
    Abort,
}

impl Plan {
    pub fn sweeping(self) -> bool {
        matches!(self, Self::Start | Self::Hold)
    }
}

pub struct PlanInputs {
    pub fallback_active: bool,
    /// The previous tick had no failed duty write and no failed RPM read.
    pub hub_healthy: bool,
    /// Latest duty the controller decided (what the hub would hold with no
    /// sweep).
    pub curve_duty: Option<u8>,
}

pub struct SelfTest {
    enabled: bool,
    stale_after_h: u64,
    duration_secs: u64,
    /// Seconds since the last settled full-tilt observation. Starts at 0 at
    /// process start, so the first possible sweep is `stale_after_h` after
    /// boot (a fresh daemon must not sweep immediately).
    since_obs_secs: u64,
    /// How long the hub has continuously held 100% duty; `None` when it
    /// does not (including after a hub re-init, via `reset_settle`).
    full_secs: Option<u64>,
    /// Seconds into the active sweep; `None` when no sweep is active.
    sweep_secs: Option<u64>,
    /// Whether this sweep produced at least one settled observation.
    sweep_observed: bool,
    /// Remaining seconds of retry backoff after an unproductive sweep.
    backoff_left: u64,
}

impl SelfTest {
    pub fn new(cfg: &SelftestConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            stale_after_h: cfg.stale_after_h,
            duration_secs: cfg.duration_secs,
            since_obs_secs: 0,
            full_secs: None,
            sweep_secs: None,
            sweep_observed: false,
            backoff_left: 0,
        }
    }

    /// Advance all timers by `elapsed` seconds. Returns true exactly when an
    /// active sweep reached `duration_secs` on this call (the sweep is over).
    pub fn begin_tick(&mut self, elapsed: u64) -> bool {
        self.since_obs_secs = self.since_obs_secs.saturating_add(elapsed);
        if let Some(f) = self.full_secs {
            self.full_secs = Some(f.saturating_add(elapsed));
        }
        if let Some(s) = self.sweep_secs {
            self.sweep_secs = Some(s.saturating_add(elapsed));
        }
        self.backoff_left = self.backoff_left.saturating_sub(elapsed);
        if self.sweep_secs >= Some(self.duration_secs) {
            self.end_sweep();
            return true;
        }
        false
    }

    /// Called after the decision block. Starts, holds or aborts a sweep.
    pub fn plan(&mut self, inputs: &PlanInputs) -> Plan {
        if self.sweep_secs.is_some() {
            if inputs.fallback_active {
                self.end_sweep();
                return Plan::Abort;
            }
            return Plan::Hold;
        }
        let start = self.enabled
            && !inputs.fallback_active
            && inputs.hub_healthy
            && inputs.curve_duty != Some(FULL_DUTY)
            && self.since_obs_secs >= self.stale_after_h * 3600
            && self.backoff_left == 0;
        if start {
            self.sweep_secs = Some(0);
            self.sweep_observed = false;
            return Plan::Start;
        }
        Plan::Idle
    }

    /// Called after the RPM reads. `hub_duty` is the duty last successfully
    /// written to the hub (None if unknown). Returns true when this tick's
    /// RPM readings are settled full-tilt samples.
    pub fn observe(&mut self, hub_duty: Option<u8>, rpm_all_ok: bool) -> bool {
        if hub_duty != Some(FULL_DUTY) {
            self.full_secs = None;
            return false;
        }
        let full = self.full_secs.get_or_insert(0);
        let settled = *full >= SELFTEST_SETTLE_SECS;
        if settled && rpm_all_ok {
            self.since_obs_secs = 0;
            self.sweep_observed = true;
        }
        // True even when rpm_all_ok is false, so the caller can still
        // record the channels that did read.
        settled
    }

    /// Hub was re-initialised: whatever it held is gone.
    pub fn reset_settle(&mut self) {
        self.full_secs = None;
    }

    fn end_sweep(&mut self) {
        self.sweep_secs = None;
        if !self.sweep_observed {
            self.backoff_left = RETRY_BACKOFF_SECS;
        }
        self.sweep_observed = false;
    }
}

/// Duty to write this tick, if any.
pub fn write_duty(sweeping: bool, decision: Option<u8>, hub_duty: Option<u8>) -> Option<u8> {
    if sweeping {
        (hub_duty != Some(FULL_DUTY)).then_some(FULL_DUTY)
    } else {
        decision
    }
}

/// Duty to feed the alarm ladder this tick.
pub fn alarm_duty(sweeping: bool, curve_duty: Option<u8>, last_applied: Option<u8>) -> u8 {
    if sweeping {
        curve_duty.unwrap_or(0)
    } else {
        last_applied.unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> SelftestConfig {
        SelftestConfig {
            enabled,
            stale_after_h: 1,
            duration_secs: 30,
        }
    }
    fn ok() -> PlanInputs {
        PlanInputs {
            fallback_active: false,
            hub_healthy: true,
            curve_duty: Some(30),
        }
    }
    /// One tick: advance, plan, then observe with the hub at 100 iff sweeping.
    fn tick(st: &mut SelfTest, elapsed: u64, rpm_ok: bool) -> (bool, Plan, bool) {
        let ended = st.begin_tick(elapsed);
        let plan = st.plan(&ok());
        let hub = if plan.sweeping() { Some(100) } else { Some(30) };
        let settled = st.observe(hub, rpm_ok);
        (ended, plan, settled)
    }

    #[test]
    fn no_sweep_before_stale_after_from_startup() {
        let mut st = SelfTest::new(&cfg(true));
        assert_eq!(tick(&mut st, 0, true).1, Plan::Idle);
        assert_eq!(tick(&mut st, 3599, true).1, Plan::Idle);
        assert_eq!(tick(&mut st, 1, true).1, Plan::Start);
    }

    #[test]
    fn disabled_never_sweeps() {
        let mut st = SelfTest::new(&cfg(false));
        assert_eq!(tick(&mut st, 1_000_000, true).1, Plan::Idle);
    }

    #[test]
    fn sweep_settles_then_expires_and_is_fresh_afterwards() {
        let mut st = SelfTest::new(&cfg(true));
        assert_eq!(tick(&mut st, 3600, true), (false, Plan::Start, false)); // t=0
        assert_eq!(tick(&mut st, 5, true), (false, Plan::Hold, false)); // t=5
        assert_eq!(tick(&mut st, 5, true), (false, Plan::Hold, true)); // t=10 settled
        assert_eq!(tick(&mut st, 5, true), (false, Plan::Hold, true)); // t=15
        assert_eq!(tick(&mut st, 5, true), (false, Plan::Hold, true)); // t=20
        assert_eq!(tick(&mut st, 5, true), (false, Plan::Hold, true)); // t=25
        assert_eq!(tick(&mut st, 5, true), (true, Plan::Idle, false)); // t=30 ended
                                                                       // Fresh for a full stale_after from the last settled sample (t=25).
        assert_eq!(tick(&mut st, 3590, true).1, Plan::Idle);
        assert_eq!(tick(&mut st, 5, true).1, Plan::Start);
    }

    #[test]
    fn sweep_without_observation_backs_off() {
        let mut st = SelfTest::new(&cfg(true));
        assert_eq!(tick(&mut st, 3600, false).1, Plan::Start);
        for _ in 0..5 {
            // settled is still true once held 10s, but nothing is recorded
            tick(&mut st, 5, false);
        }
        // Expires at t=30; the 3600s backoff starts here, and the measurement
        // is still stale, so only the backoff is holding the next sweep off.
        assert_eq!(tick(&mut st, 5, false), (true, Plan::Idle, false));
        assert_eq!(tick(&mut st, 3595, true).1, Plan::Idle); // 5s of backoff left
        assert_eq!(tick(&mut st, 5, true).1, Plan::Start); // backoff over
    }

    #[test]
    fn fallback_aborts_an_active_sweep() {
        let mut st = SelfTest::new(&cfg(true));
        assert_eq!(tick(&mut st, 3600, true).1, Plan::Start);
        st.begin_tick(5);
        let fb = PlanInputs {
            fallback_active: true,
            ..ok()
        };
        assert_eq!(st.plan(&fb), Plan::Abort);
        assert_eq!(st.plan(&fb), Plan::Idle);
        // Aborted before any settled sample: backoff applies.
        st.begin_tick(5);
        assert_eq!(st.plan(&ok()), Plan::Idle);
    }

    #[test]
    fn does_not_start_during_fallback_unhealthy_hub_or_curve_at_100() {
        for inputs in [
            PlanInputs {
                fallback_active: true,
                ..ok()
            },
            PlanInputs {
                hub_healthy: false,
                ..ok()
            },
            PlanInputs {
                curve_duty: Some(100),
                ..ok()
            },
        ] {
            let mut st = SelfTest::new(&cfg(true));
            st.begin_tick(3600);
            assert_eq!(st.plan(&inputs), Plan::Idle);
        }
    }

    #[test]
    fn starts_when_curve_duty_unknown() {
        let mut st = SelfTest::new(&cfg(true));
        st.begin_tick(3600);
        assert_eq!(
            st.plan(&PlanInputs {
                curve_duty: None,
                ..ok()
            }),
            Plan::Start
        );
    }

    #[test]
    fn passive_observation_at_100_refreshes_the_clock_even_when_disabled() {
        for enabled in [false, true] {
            let mut st = SelfTest::new(&cfg(enabled));
            st.begin_tick(3000);
            assert!(!st.observe(Some(100), true)); // held 0s
            st.begin_tick(5);
            assert!(!st.observe(Some(100), true)); // held 5s
            st.begin_tick(5);
            assert!(st.observe(Some(100), true)); // held 10s: settled, clock reset
            st.begin_tick(3599);
            assert_eq!(st.plan(&ok()), Plan::Idle);
        }
    }

    #[test]
    fn duty_below_100_is_never_a_full_tilt_sample() {
        let mut st = SelfTest::new(&cfg(true));
        for duty in [Some(95), Some(99), None] {
            st.begin_tick(60);
            assert!(!st.observe(duty, true));
        }
    }

    #[test]
    fn leaving_100_restarts_the_settle_clock() {
        let mut st = SelfTest::new(&cfg(true));
        st.observe(Some(100), true);
        st.begin_tick(10);
        assert!(st.observe(Some(100), true));
        st.begin_tick(5);
        assert!(!st.observe(Some(90), true));
        st.begin_tick(5);
        assert!(!st.observe(Some(100), true)); // held 0s again
    }

    #[test]
    fn reset_settle_restarts_the_settle_clock() {
        let mut st = SelfTest::new(&cfg(true));
        st.observe(Some(100), true);
        st.begin_tick(10);
        assert!(st.observe(Some(100), true));
        st.reset_settle();
        st.begin_tick(5);
        assert!(!st.observe(Some(100), true)); // held 0s after re-init
        st.begin_tick(10);
        assert!(st.observe(Some(100), true));
    }

    #[test]
    fn write_duty_overrides_only_while_sweeping() {
        assert_eq!(write_duty(true, Some(40), Some(30)), Some(100));
        assert_eq!(write_duty(true, None, Some(30)), Some(100));
        assert_eq!(write_duty(true, Some(40), Some(100)), None); // already there
    }

    #[test]
    fn write_duty_rewrites_100_when_hub_duty_unknown() {
        assert_eq!(write_duty(true, None, None), Some(100));
    }

    #[test]
    fn write_duty_passes_decision_through_when_not_sweeping() {
        assert_eq!(write_duty(false, Some(60), Some(100)), Some(60));
        assert_eq!(write_duty(false, None, None), None);
        assert_eq!(write_duty(false, None, Some(100)), None);
    }

    #[test]
    fn alarm_duty_hides_the_sweep_but_not_the_curve() {
        assert_eq!(alarm_duty(true, Some(30), Some(100)), 30);
        assert_eq!(alarm_duty(true, Some(100), Some(100)), 100); // curve really pinned
        assert_eq!(alarm_duty(true, None, Some(100)), 0);
        assert_eq!(alarm_duty(false, Some(30), Some(90)), 90);
        assert_eq!(alarm_duty(false, None, None), 0);
    }
}
