use crate::signals::{SignalKind, ThrottleFlags};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
struct State {
    temps: BTreeMap<&'static str, f64>,
    duty: BTreeMap<u8, u8>,
    rpm: BTreeMap<u8, u16>,
    degraded: bool,
    fallback_active: bool,
    hub_present: bool,
    hid_errors_write: u64,
    hid_errors_read: u64,
    rgb_errors: u64,
    led_state: u8,
    last_tick: u64,

    // Multi-signal fusion (Wave 5). Keyed by SignalKind::as_str(); a key is
    // only ever present once the corresponding setter has been called for
    // that signal this run, so an unavailable signal emits no series at all
    // (matching how `temps` already behaves when a sensor is missing).
    signal_values: BTreeMap<&'static str, (f64, &'static str)>,
    signal_candidate_duty: BTreeMap<&'static str, u8>,
    control_signal: Option<&'static str>,
    gpu_power_limit_w: Option<f64>,
    throttle: Option<ThrottleFlags>,
    throttle_floor_active: bool,
    signal_errors: BTreeMap<&'static str, u64>,
}

pub struct Metrics {
    state: Mutex<State>,
    /// Fixed for the process lifetime; used by the /healthz staleness check.
    poll_interval_secs: u64,
}

/// Current Unix time in whole seconds. Clamped to 0 on a pre-1970 clock
/// (never in practice, but keeps this infallible for callers).
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pure threshold logic behind `/healthz`: healthy iff the last completed
/// tick is no more than `3 * poll_secs` old. `last_tick` is seeded to
/// process-start time at construction, so this also grants a startup grace
/// window before the first real tick completes.
pub fn healthz_ok(now_secs: u64, last_tick_secs: u64, poll_secs: u64) -> bool {
    now_secs.saturating_sub(last_tick_secs) <= 3 * poll_secs
}

impl Metrics {
    pub fn new(poll_interval_secs: u64) -> Arc<Metrics> {
        Arc::new(Metrics {
            state: Mutex::new(State {
                // Seed to "now" so /healthz reads healthy from process start
                // through the first 3x poll grace window, even before the
                // control loop (or the hub-wait retry loop) completes a tick.
                last_tick: now_unix_secs(),
                ..State::default()
            }),
            poll_interval_secs,
        })
    }

    /// Lock the state mutex, recovering from poisoning instead of propagating
    /// it. A panic on the metrics-serving thread while holding this lock
    /// (there is no realistic path for one today, but the *next* accessor on
    /// the control loop must never die because of it) must not cascade into
    /// killing fan control — the state is plain POD gauges/counters, so a
    /// torn update is harmless to recover from.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_temp(&self, source: &'static str, v: f64) {
        self.state().temps.insert(source, v);
    }
    pub fn set_duty(&self, ch: u8, v: u8) {
        self.state().duty.insert(ch, v);
    }
    pub fn set_rpm(&self, ch: u8, v: u16) {
        self.state().rpm.insert(ch, v);
    }
    pub fn set_degraded(&self, v: bool) {
        self.state().degraded = v;
    }
    pub fn set_fallback_active(&self, v: bool) {
        self.state().fallback_active = v;
    }
    pub fn set_hub_present(&self, v: bool) {
        self.state().hub_present = v;
    }
    pub fn inc_hid_errors_write(&self) {
        self.state().hid_errors_write += 1;
    }
    pub fn inc_hid_errors_read(&self) {
        self.state().hid_errors_read += 1;
    }
    pub fn inc_rgb_errors(&self) {
        self.state().rgb_errors += 1;
    }
    pub fn set_led_state(&self, v: u8) {
        self.state().led_state = v;
    }
    /// Set at the end of every control-loop tick (and, while waiting for the
    /// hub at startup, refreshed on that retry cadence too) so `/healthz`
    /// reflects real progress rather than a wedged loop.
    pub fn set_last_tick(&self, unix_secs: u64) {
        self.state().last_tick = unix_secs;
    }

    /// Conditioned (smoothed) value of one available input signal.
    #[allow(dead_code)] // wired by Wave 7's control-loop integration
    pub fn set_signal_value(&self, signal: SignalKind, v: f64) {
        self.state()
            .signal_values
            .insert(signal.as_str(), (v, signal.unit()));
    }
    /// Duty this signal's own curve would command this tick, independent of
    /// whether it ends up winning the max-of-candidates fusion.
    #[allow(dead_code)] // wired by Wave 7's control-loop integration
    pub fn set_candidate_duty(&self, signal: SignalKind, duty: u8) {
        self.state()
            .signal_candidate_duty
            .insert(signal.as_str(), duty);
    }
    /// The signal whose candidate duty drove the applied duty this tick.
    /// Rendered one-hot against every currently-available signal (i.e. every
    /// signal with a candidate duty registered) rather than as a single
    /// info-series whose label value changes, which would leave stale
    /// timeseries behind and break rate()/graphing across a hand-off.
    #[allow(dead_code)] // wired by Wave 7's control-loop integration
    pub fn set_control_signal(&self, winner: SignalKind) {
        self.state().control_signal = Some(winner.as_str());
    }
    /// Enforced GPU power limit, when known. Enables percent-of-limit
    /// dashboards regardless of which unit the power curve itself uses.
    #[allow(dead_code)] // wired by Wave 7's control-loop integration
    pub fn set_gpu_power_limit_watts(&self, v: f64) {
        self.state().gpu_power_limit_w = Some(v);
    }
    /// Snapshot of NVML throttle-reason bits for this tick.
    #[allow(dead_code)] // wired by Wave 7's control-loop integration
    pub fn set_throttle(&self, flags: ThrottleFlags) {
        self.state().throttle = Some(flags);
    }
    /// 1 while the throttle floor is raising the applied duty above what the
    /// fused signals alone would command (including its hold window).
    #[allow(dead_code)] // wired by Wave 7's control-loop integration
    pub fn set_throttle_floor_active(&self, v: bool) {
        self.state().throttle_floor_active = v;
    }
    /// A REAL read failure for this signal (not `NotSupported`, which means
    /// the signal is simply absent from this card/driver).
    #[allow(dead_code)] // wired by Wave 7's control-loop integration
    pub fn inc_signal_error(&self, signal: SignalKind) {
        *self
            .state()
            .signal_errors
            .entry(signal.as_str())
            .or_insert(0) += 1;
    }

    fn healthz(&self) -> bool {
        let last_tick = self.state().last_tick;
        healthz_ok(now_unix_secs(), last_tick, self.poll_interval_secs)
    }

    pub fn render(&self) -> String {
        let s = self.state();
        let mut out = String::new();
        out.push_str("# HELP unifand_temp_celsius Sensor and control temperatures.\n# TYPE unifand_temp_celsius gauge\n");
        for (src, v) in &s.temps {
            out.push_str(&format!(
                "unifand_temp_celsius{{source=\"{}\"}} {}\n",
                src, v
            ));
        }
        out.push_str("# HELP unifand_duty_percent Last duty written per channel.\n# TYPE unifand_duty_percent gauge\n");
        for (ch, v) in &s.duty {
            out.push_str(&format!(
                "unifand_duty_percent{{channel=\"{}\"}} {}\n",
                ch, v
            ));
        }
        out.push_str(
            "# HELP unifand_fan_rpm Fan speed per channel.\n# TYPE unifand_fan_rpm gauge\n",
        );
        for (ch, v) in &s.rpm {
            out.push_str(&format!("unifand_fan_rpm{{channel=\"{}\"}} {}\n", ch, v));
        }
        out.push_str("# HELP unifand_degraded 1 when GPU sensor unavailable.\n# TYPE unifand_degraded gauge\n");
        out.push_str(&format!(
            "unifand_degraded {}\n",
            if s.degraded { 1 } else { 0 }
        ));
        out.push_str("# HELP unifand_fallback_active 1 when all sensors are lost and fallback_duty is being forced.\n# TYPE unifand_fallback_active gauge\n");
        out.push_str(&format!(
            "unifand_fallback_active {}\n",
            if s.fallback_active { 1 } else { 0 }
        ));
        out.push_str("# HELP unifand_hub_present 1 when the SL V2 hub is currently open and initialized.\n# TYPE unifand_hub_present gauge\n");
        out.push_str(&format!(
            "unifand_hub_present {}\n",
            if s.hub_present { 1 } else { 0 }
        ));
        out.push_str("# HELP unifand_hid_errors_total HID write/read failures, by kind.\n# TYPE unifand_hid_errors_total counter\n");
        out.push_str(&format!(
            "unifand_hid_errors_total{{kind=\"write\"}} {}\n",
            s.hid_errors_write
        ));
        out.push_str(&format!(
            "unifand_hid_errors_total{{kind=\"read\"}} {}\n",
            s.hid_errors_read
        ));
        out.push_str("# HELP unifand_rgb_errors_total RGB write failures.\n# TYPE unifand_rgb_errors_total counter\n");
        out.push_str(&format!("unifand_rgb_errors_total {}\n", s.rgb_errors));
        out.push_str("# HELP unifand_led_state Alarm ladder state (0 normal, 1 sustained-hot, 2 near-limit, 3 fault).\n# TYPE unifand_led_state gauge\n");
        out.push_str(&format!("unifand_led_state {}\n", s.led_state));

        // --- Multi-signal fusion (Wave 5, strictly additive) ---
        // Absent signal => no series, matching `temps` above: these maps are
        // only ever populated for signals that are actually available.
        if !s.signal_values.is_empty() {
            out.push_str("# HELP unifand_signal_value Conditioned (smoothed) value of each available input signal.\n# TYPE unifand_signal_value gauge\n");
            for (signal, (v, unit)) in &s.signal_values {
                out.push_str(&format!(
                    "unifand_signal_value{{signal=\"{}\",unit=\"{}\"}} {}\n",
                    signal, unit, v
                ));
            }
        }
        if !s.signal_candidate_duty.is_empty() {
            out.push_str("# HELP unifand_signal_candidate_duty_percent Duty each available signal's curve commands this tick.\n# TYPE unifand_signal_candidate_duty_percent gauge\n");
            for (signal, duty) in &s.signal_candidate_duty {
                out.push_str(&format!(
                    "unifand_signal_candidate_duty_percent{{signal=\"{}\"}} {}\n",
                    signal, duty
                ));
            }
            out.push_str("# HELP unifand_control_signal One-hot: 1 for the signal driving the applied duty, 0 for other available signals.\n# TYPE unifand_control_signal gauge\n");
            for signal in s.signal_candidate_duty.keys() {
                let v = if s.control_signal == Some(*signal) {
                    1
                } else {
                    0
                };
                out.push_str(&format!(
                    "unifand_control_signal{{signal=\"{}\"}} {}\n",
                    signal, v
                ));
            }
        }
        if let Some(limit) = s.gpu_power_limit_w {
            out.push_str("# HELP unifand_gpu_power_limit_watts Enforced GPU power limit; enables percent-of-limit dashboards regardless of curve unit.\n# TYPE unifand_gpu_power_limit_watts gauge\n");
            out.push_str(&format!("unifand_gpu_power_limit_watts {}\n", limit));
        }
        if let Some(t) = &s.throttle {
            out.push_str("# HELP unifand_throttle_active NVML throttle reason currently asserted.\n# TYPE unifand_throttle_active gauge\n");
            for (reason, active) in [
                ("sw_thermal", t.sw_thermal),
                ("hw_thermal", t.hw_thermal),
                ("hw_power_brake", t.hw_power_brake),
                ("sw_power_cap", t.sw_power_cap),
            ] {
                out.push_str(&format!(
                    "unifand_throttle_active{{reason=\"{}\"}} {}\n",
                    reason,
                    if active { 1 } else { 0 }
                ));
            }
        }
        out.push_str("# HELP unifand_throttle_floor_active 1 while the throttle floor is raising applied duty (including hold window).\n# TYPE unifand_throttle_floor_active gauge\n");
        out.push_str(&format!(
            "unifand_throttle_floor_active {}\n",
            if s.throttle_floor_active { 1 } else { 0 }
        ));
        if !s.signal_errors.is_empty() {
            out.push_str("# HELP unifand_signal_errors_total Real read failures per signal; NotSupported is not counted (it means the signal is absent).\n# TYPE unifand_signal_errors_total counter\n");
            for (signal, count) in &s.signal_errors {
                out.push_str(&format!(
                    "unifand_signal_errors_total{{signal=\"{}\"}} {}\n",
                    signal, count
                ));
            }
        }

        out.push_str("# HELP unifand_last_tick_timestamp_seconds Unix time the control loop last completed a tick (or startup hub-wait progress).\n# TYPE unifand_last_tick_timestamp_seconds gauge\n");
        out.push_str(&format!(
            "unifand_last_tick_timestamp_seconds {}\n",
            s.last_tick
        ));
        out.push_str(
            "# HELP unifand_build_info Build metadata, value is always 1.\n# TYPE unifand_build_info gauge\n",
        );
        out.push_str(&format!(
            "unifand_build_info{{version=\"{}\"}} 1\n",
            env!("CARGO_PKG_VERSION")
        ));
        out
    }
}

pub fn serve(m: Arc<Metrics>, listen: String) {
    std::thread::spawn(move || {
        let server = match tiny_http::Server::http(&listen) {
            Ok(s) => s,
            Err(e) => {
                log::error!("metrics server failed to bind {}: {}", listen, e);
                return;
            }
        };
        log::info!("metrics listening on {}", listen);
        for req in server.incoming_requests() {
            if req.url() == "/healthz" {
                let (code, body) = if m.healthz() {
                    (200, "ok\n")
                } else {
                    (500, "stale\n")
                };
                let resp = tiny_http::Response::from_string(body)
                    .with_status_code(tiny_http::StatusCode(code));
                let _ = req.respond(resp);
                continue;
            }
            let body = m.render();
            let resp = tiny_http::Response::from_string(body).with_header(
                tiny_http::Header::from_bytes(
                    &b"Content-Type"[..],
                    &b"text/plain; version=0.0.4"[..],
                )
                .unwrap(),
            );
            let _ = req.respond(resp);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Arc<Metrics> {
        Metrics::new(5)
    }

    #[test]
    fn renders_prometheus_text() {
        let m = m();
        m.set_temp("gpu", 71.0);
        m.set_temp("control", 71.0);
        m.set_duty(1, 65);
        m.set_rpm(1, 1350);
        m.set_degraded(false);
        m.inc_hid_errors_write();
        m.inc_hid_errors_write();
        m.inc_hid_errors_read();
        let out = m.render();
        assert!(out.contains("# TYPE unifand_temp_celsius gauge"));
        assert!(out.contains("unifand_temp_celsius{source=\"gpu\"} 71"));
        assert!(out.contains("unifand_duty_percent{channel=\"1\"} 65"));
        assert!(out.contains("unifand_fan_rpm{channel=\"1\"} 1350"));
        assert!(out.contains("unifand_degraded 0"));
        assert!(out.contains("unifand_hid_errors_total{kind=\"write\"} 2"));
        assert!(out.contains("unifand_hid_errors_total{kind=\"read\"} 1"));
    }

    #[test]
    fn degraded_flag_renders_one() {
        let m = m();
        m.set_degraded(true);
        assert!(m.render().contains("unifand_degraded 1"));
    }

    #[test]
    fn fallback_active_renders() {
        let m = m();
        assert!(m.render().contains("unifand_fallback_active 0"));
        m.set_fallback_active(true);
        let out = m.render();
        assert!(out.contains("# TYPE unifand_fallback_active gauge"));
        assert!(out.contains("unifand_fallback_active 1"));
    }

    #[test]
    fn hub_present_renders() {
        let m = m();
        assert!(m.render().contains("unifand_hub_present 0"));
        m.set_hub_present(true);
        assert!(m.render().contains("unifand_hub_present 1"));
    }

    #[test]
    fn build_info_renders_crate_version() {
        let out = m().render();
        assert!(out.contains("# TYPE unifand_build_info gauge"));
        assert!(out.contains(&format!(
            "unifand_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
    }

    #[test]
    fn last_tick_seeded_near_process_start_and_settable() {
        let m = m();
        let seeded = now_unix_secs();
        let out = m.render();
        assert!(out.contains("# TYPE unifand_last_tick_timestamp_seconds gauge"));
        // Seeded value must be close to "now" (within a couple seconds of
        // test execution), not zero.
        let line = out
            .lines()
            .find(|l| l.starts_with("unifand_last_tick_timestamp_seconds "))
            .unwrap();
        let v: u64 = line
            .strip_prefix("unifand_last_tick_timestamp_seconds ")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(v.abs_diff(seeded) <= 2);

        m.set_last_tick(1_000);
        assert!(m
            .render()
            .contains("unifand_last_tick_timestamp_seconds 1000"));
    }

    #[test]
    fn rgb_errors_counter_renders() {
        let m = m();
        m.inc_rgb_errors();
        let out = m.render();
        assert!(out.contains("# TYPE unifand_rgb_errors_total counter"));
        assert!(out.contains("unifand_rgb_errors_total 1"));
    }

    #[test]
    fn led_state_renders() {
        let m = m();
        m.set_led_state(2);
        let out = m.render();
        assert!(out.contains(
            "# HELP unifand_led_state Alarm ladder state (0 normal, 1 sustained-hot, 2 near-limit, 3 fault)."
        ));
        assert!(out.contains("# TYPE unifand_led_state gauge"));
        assert!(out.contains("unifand_led_state 2"));
    }

    #[test]
    fn healthz_ok_within_3x_poll_interval() {
        assert!(healthz_ok(100, 100, 5)); // exactly now
        assert!(healthz_ok(115, 100, 5)); // exactly at the 3x boundary
        assert!(!healthz_ok(116, 100, 5)); // one second past
    }

    #[test]
    fn healthz_ok_handles_clock_seed_before_now() {
        // last_tick in the future (clock skew) must not panic (saturating_sub).
        assert!(healthz_ok(100, 200, 5));
    }

    #[test]
    fn healthz_ok_zero_poll_interval_requires_exact_match() {
        assert!(healthz_ok(100, 100, 0));
        assert!(!healthz_ok(101, 100, 0));
    }

    #[test]
    fn signal_value_renders_with_unit_label() {
        let m = m();
        m.set_signal_value(SignalKind::GpuPower, 412.5);
        let out = m.render();
        assert!(out.contains("# HELP unifand_signal_value"));
        assert!(out.contains("# TYPE unifand_signal_value gauge"));
        assert!(out.contains("unifand_signal_value{signal=\"gpu_power\",unit=\"watts\"} 412.5"));
    }

    #[test]
    fn candidate_duty_renders_per_signal() {
        let m = m();
        m.set_candidate_duty(SignalKind::GpuTemp, 55);
        m.set_candidate_duty(SignalKind::GpuPower, 80);
        let out = m.render();
        assert!(out.contains("# TYPE unifand_signal_candidate_duty_percent gauge"));
        assert!(out.contains("unifand_signal_candidate_duty_percent{signal=\"gpu_temp\"} 55"));
        assert!(out.contains("unifand_signal_candidate_duty_percent{signal=\"gpu_power\"} 80"));
    }

    #[test]
    fn control_signal_is_one_hot() {
        let m = m();
        // Two available signals (both have a candidate duty); a third
        // (mem_temp) is never registered, i.e. unavailable this run.
        m.set_candidate_duty(SignalKind::GpuPower, 80);
        m.set_candidate_duty(SignalKind::GpuTemp, 55);
        m.set_control_signal(SignalKind::GpuPower);
        let out = m.render();
        assert!(out.contains("# TYPE unifand_control_signal gauge"));
        assert!(out.contains("unifand_control_signal{signal=\"gpu_power\"} 1"));
        assert!(out.contains("unifand_control_signal{signal=\"gpu_temp\"} 0"));
        assert!(!out.contains("signal=\"mem_temp\""));
    }

    #[test]
    fn absent_signal_emits_no_series() {
        let m = m();
        m.set_signal_value(SignalKind::GpuPower, 100.0);
        m.set_candidate_duty(SignalKind::GpuPower, 60);
        m.set_control_signal(SignalKind::GpuPower);
        let out = m.render();
        // mem_temp is never touched -> absent, not zero.
        assert!(!out.contains("signal=\"mem_temp\""));
    }

    #[test]
    fn throttle_metrics_render_all_four_reasons() {
        let m = m();
        m.set_throttle(ThrottleFlags {
            sw_thermal: true,
            hw_thermal: false,
            hw_power_brake: false,
            sw_power_cap: true,
        });
        let out = m.render();
        assert!(out.contains("# TYPE unifand_throttle_active gauge"));
        assert!(out.contains("unifand_throttle_active{reason=\"sw_thermal\"} 1"));
        assert!(out.contains("unifand_throttle_active{reason=\"hw_thermal\"} 0"));
        assert!(out.contains("unifand_throttle_active{reason=\"hw_power_brake\"} 0"));
        assert!(out.contains("unifand_throttle_active{reason=\"sw_power_cap\"} 1"));
    }

    #[test]
    fn legacy_render_is_unchanged_when_no_signals_registered() {
        let m = m();
        m.set_temp("gpu", 71.0);
        m.set_duty(1, 65);
        m.set_rpm(1, 1350);
        m.set_degraded(false);
        m.set_led_state(0);
        let out = m.render();
        // Cover every new family, not just the `unifand_signal_` prefix: three
        // of them (control_signal, gpu_power_limit_watts, throttle_active)
        // don't share it, so a prefix-only check would let an unconditionally
        // rendered one slip past a test whose name promises otherwise.
        for family in [
            "unifand_signal_value",
            "unifand_signal_candidate_duty_percent",
            "unifand_signal_errors_total",
            "unifand_control_signal",
            "unifand_gpu_power_limit_watts",
            "unifand_throttle_active",
        ] {
            assert!(
                !out.contains(family),
                "legacy render must not emit {family}, got:\n{out}"
            );
        }
        // The one sanctioned addition: a single always-present scalar. Pinned
        // here so "strictly additive plus this one line" stays a deliberate,
        // reviewed choice rather than drift.
        assert!(out.contains("unifand_throttle_floor_active 0"));
    }
}
