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
}
