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
        let next = match self.last_duty {
            None => target,
            Some(last) => {
                let temp_moved = (temp - self.last_temp).abs() >= self.hysteresis_c;
                let duty_gap = target.abs_diff(last);
                if !temp_moved && duty_gap < self.min_duty_delta {
                    return None;
                }
                if duty_gap == 0 {
                    self.last_temp = temp;
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
        self.last_temp = temp;
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
}
