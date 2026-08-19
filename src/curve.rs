use crate::config::CurvePoint;

/// Piecewise-linear interpolation over a sorted curve, clamped at both ends.
pub fn interpolate(curve: &[CurvePoint], temp: f64) -> u8 {
    debug_assert!(!curve.is_empty());
    if temp <= curve[0].temp {
        return curve[0].duty;
    }
    let last = curve[curve.len() - 1];
    if temp >= last.temp {
        return last.duty;
    }
    for w in curve.windows(2) {
        let (a, b) = (w[0], w[1]);
        if temp <= b.temp {
            let frac = (temp - a.temp) / (b.temp - a.temp);
            let duty = a.duty as f64 + frac * (b.duty as f64 - a.duty as f64);
            return duty.round() as u8;
        }
    }
    last.duty
}

/// Decides when and how far to move fan duty: hysteresis suppresses noise,
/// slew limiting smooths ramps.
pub struct Controller {
    hysteresis_c: f64,
    min_duty_delta: u8,
    max_step: u8,
    last_duty: Option<u8>,
    last_temp: f64,
}

impl Controller {
    pub fn new(hysteresis_c: f64, min_duty_delta: u8, max_step: u8) -> Self {
        Self {
            hysteresis_c,
            min_duty_delta,
            max_step,
            last_duty: None,
            last_temp: f64::NAN,
        }
    }

    pub fn force(&mut self, duty: u8) {
        self.last_duty = Some(duty);
    }

    pub fn decide(&mut self, curve: &[CurvePoint], temp: f64) -> Option<u8> {
        let target = interpolate(curve, temp);
        self.step_toward(target, Some(temp))
    }

    /// Drives the controller straight off a fused duty target (e.g. from
    /// `signals::fuse`) instead of a temperature/curve lookup. `temp: None`
    /// means `step_toward`'s `temp_moved` gate is always false and
    /// `last_temp` is never read or written, so the hysteresis hold
    /// degenerates to a pure `duty_gap >= min_duty_delta` check in duty
    /// space.
    ///
    /// Not yet wired into the control loop (Wave 7 does that; only this
    /// module's own tests call it so far), hence `allow(dead_code)`.
    #[allow(dead_code)]
    pub fn decide_target(&mut self, target: u8) -> Option<u8> {
        self.step_toward(target, None)
    }

    /// Today's `decide` control flow, extracted verbatim: hysteresis hold,
    /// slew limiting, and their two asymmetric early returns. `temp: Some`
    /// enables the hysteresis "hold if neither temp nor duty moved enough"
    /// gate and updates `last_temp` on every path that doesn't hold; `temp:
    /// None` disables the temp-moved gate entirely (see `decide_target`)
    /// and never touches `last_temp`.
    ///
    /// LOAD-BEARING ASYMMETRY (pinned by the 10 pre-existing curve tests):
    /// the hysteresis hold returns `None` WITHOUT updating `last_temp`,
    /// while the `duty_gap == 0` branch DOES update `last_temp` before
    /// returning `None`.
    fn step_toward(&mut self, target: u8, temp: Option<f64>) -> Option<u8> {
        let next = match self.last_duty {
            None => target,
            Some(last) => {
                let temp_moved = match temp {
                    Some(t) => (t - self.last_temp).abs() >= self.hysteresis_c,
                    None => false,
                };
                let duty_gap = target.abs_diff(last);
                if !temp_moved && duty_gap < self.min_duty_delta {
                    return None;
                }
                if duty_gap == 0 {
                    if let Some(t) = temp {
                        self.last_temp = t;
                    }
                    return None;
                }
                let step = duty_gap.min(self.max_step);
                if target > last {
                    last + step
                } else {
                    last - step
                }
            }
        };
        self.last_duty = Some(next);
        if let Some(t) = temp {
            self.last_temp = t;
        }
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CurvePoint;

    fn c() -> Vec<CurvePoint> {
        vec![
            CurvePoint {
                temp: 35.0,
                duty: 30,
            },
            CurvePoint {
                temp: 55.0,
                duty: 40,
            },
            CurvePoint {
                temp: 80.0,
                duty: 100,
            },
        ]
    }

    #[test]
    fn clamps_below_first_point() {
        assert_eq!(interpolate(&c(), 20.0), 30);
    }

    #[test]
    fn clamps_above_last_point() {
        assert_eq!(interpolate(&c(), 95.0), 100);
    }

    #[test]
    fn exact_point() {
        assert_eq!(interpolate(&c(), 55.0), 40);
    }

    #[test]
    fn midpoint_interpolates_linearly() {
        // halfway 35..55 -> halfway 30..40 = 35
        assert_eq!(interpolate(&c(), 45.0), 35);
        // halfway 55..80 -> halfway 40..100 = 70
        assert_eq!(interpolate(&c(), 67.5), 70);
    }

    #[test]
    fn single_point_curve_is_constant() {
        let one = vec![CurvePoint {
            temp: 50.0,
            duty: 55,
        }];
        assert_eq!(interpolate(&one, 10.0), 55);
        assert_eq!(interpolate(&one, 90.0), 55);
    }

    #[test]
    fn first_decision_applies_target_directly() {
        let mut ctl = Controller::new(2.0, 5, 10);
        assert_eq!(ctl.decide(&c(), 45.0), Some(35));
    }

    #[test]
    fn small_changes_are_held_by_hysteresis() {
        let mut ctl = Controller::new(2.0, 5, 10);
        ctl.decide(&c(), 45.0); // applied 35
                                // +1C and duty delta < 5: hold
        assert_eq!(ctl.decide(&c(), 46.0), None);
    }

    #[test]
    fn big_temp_move_ramps_with_slew_limit() {
        let mut ctl = Controller::new(2.0, 5, 10);
        ctl.decide(&c(), 45.0); // 35
                                // target at 80C is 100, but slew caps at 35+10
        assert_eq!(ctl.decide(&c(), 80.0), Some(45));
        // temp static, ramp continues toward 100
        assert_eq!(ctl.decide(&c(), 80.0), Some(55));
    }

    #[test]
    fn ramp_down_is_also_slew_limited() {
        let mut ctl = Controller::new(2.0, 5, 10);
        ctl.decide(&c(), 80.0); // 100
        assert_eq!(ctl.decide(&c(), 35.0), Some(90));
    }

    #[test]
    fn force_updates_state() {
        let mut ctl = Controller::new(2.0, 5, 10);
        ctl.force(60);
        // target 35 at 45C; from 60 slew-limited to 50
        assert_eq!(ctl.decide(&c(), 45.0), Some(50));
    }

    // -- decide_target / step_toward ---------------------------------------

    #[test]
    fn decide_target_first_call_applies_directly() {
        let mut ctl = Controller::new(2.0, 5, 10);
        assert_eq!(ctl.decide_target(35), Some(35));
    }

    #[test]
    fn decide_target_holds_within_min_duty_delta() {
        let mut ctl = Controller::new(2.0, 5, 10);
        ctl.decide_target(35);
        // duty_gap = |38-35| = 3 < min_duty_delta(5), and temp_moved is
        // always false when temp is None: hold.
        assert_eq!(ctl.decide_target(38), None);
    }

    #[test]
    fn decide_target_is_slew_limited() {
        let mut ctl = Controller::new(2.0, 5, 10);
        ctl.decide_target(35);
        assert_eq!(ctl.decide_target(100), Some(45));
        assert_eq!(ctl.decide_target(100), Some(55));
    }

    #[test]
    fn decide_target_duty_gap_zero_does_not_touch_last_temp() {
        // With any positive min_duty_delta, duty_gap == 0 always satisfies
        // `duty_gap < min_duty_delta` (since temp_moved is unconditionally
        // false when temp is None), so the FIRST early return (hysteresis
        // hold) always intercepts before the second (`duty_gap == 0`)
        // branch is ever reached. min_duty_delta 0 is required to reach it:
        // duty_gap (unsigned) can never be < 0.
        let mut ctl = Controller::new(2.0, 0, 10);
        assert_eq!(ctl.decide_target(35), Some(35));
        // Same target again: duty_gap == 0, falls through the first branch
        // (0 < 0 is false) and hits the second -- must return None WITHOUT
        // assigning last_temp, since temp is None throughout decide_target.
        assert_eq!(ctl.decide_target(35), None);
        assert!(
            ctl.last_temp.is_nan(),
            "decide_target must never assign last_temp; got {}",
            ctl.last_temp
        );
    }

    #[test]
    fn decide_and_decide_target_agree_on_the_same_curve_target() {
        // Interleave decide() and decide_target() on the same controller to
        // prove the step_toward extraction didn't alter the temp-driven
        // path: decide_target() must not read or write last_temp, so a
        // subsequent decide() call must behave exactly as if the
        // decide_target() call had never happened.
        let mut ctl = Controller::new(2.0, 5, 10);
        assert_eq!(ctl.decide(&c(), 45.0), Some(35)); // last_duty=35, last_temp=45

        let target = interpolate(&c(), 80.0); // 100
        assert_eq!(ctl.decide_target(target), Some(45)); // slew-limited 35->45; last_temp untouched

        // last_temp is still 45.0 here (35 apart from 80 -> temp_moved),
        // and last_duty is 45: duty_gap to target 100 is 55, slew-capped
        // at 10 -> 55.
        assert_eq!(ctl.decide(&c(), 80.0), Some(55));
    }
}
