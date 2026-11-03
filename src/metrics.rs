use crate::signals::{GpuReading, SignalKind, ThrottleFlags};
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

    // Multi-signal fusion (Wave 5). Keyed by SignalKind::as_str().
    // `signal_values`, `signal_candidate_duty` and `control_signal` are
    // replaced wholesale each fusion tick by `set_fusion_frame`, so they
    // describe the LATEST tick only: a signal that goes away stops emitting
    // rather than freezing at its last value. (`temps` above is still
    // insert-only -- legacy behavior, deliberately left alone.)
    signal_values: BTreeMap<&'static str, (f64, &'static str)>,
    signal_candidate_duty: BTreeMap<&'static str, u8>,
    control_signal: Option<&'static str>,
    gpu_power_limit_w: Option<f64>,
    throttle: Option<ThrottleFlags>,
    throttle_floor_active: bool,
    signal_errors: BTreeMap<&'static str, u64>,

    // Per-card view, replaced wholesale each tick by `set_gpu_frame`.
    gpus: Vec<GpuReading>,
    gpu_read_errors: BTreeMap<u32, u64>,
}

/// Escapes a Prometheus label value (backslash, double quote, newline).
/// Only needed for the card name, which is whatever NVML reports.
fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Appends one per-card gauge family, omitting the family entirely when no
/// card has the reading and omitting individual cards that lack it.
fn push_gpu_gauge(
    out: &mut String,
    gpus: &[GpuReading],
    name: &str,
    help: &str,
    pick: fn(&GpuReading) -> Option<f64>,
) {
    if !gpus.iter().any(|g| pick(g).is_some()) {
        return;
    }
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
    for g in gpus {
        if let Some(v) = pick(g) {
            out.push_str(&format!("{name}{{gpu=\"{}\"}} {}\n", g.index, v));
        }
    }
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

    /// Publishes this tick's entire fusion view in one atomic update:
    /// conditioned signal values, per-signal candidate duties, and the
    /// winning signal. Both arrays are indexed by `SignalKind::idx()`,
    /// matching `signals::Conditioned::values` and `signals::Fusion`.
    ///
    /// REPLACES rather than merges, and that is the whole point. A signal
    /// that was present on an earlier tick but is absent now leaves no
    /// stale series behind, and `winner: None` — a floor-only tick, or
    /// total signal loss — clears the one-hot instead of leaving the
    /// previous winner reading 1, which is exactly the degraded condition
    /// where someone is staring at the dashboard. Taking all three together
    /// in one call makes that structural: there is no ordering of separate
    /// setters a caller can get wrong, and no torn frame is observable to a
    /// concurrent scrape.
    ///
    /// The one-hot renders across every signal with a candidate duty in
    /// THIS frame, rather than as one info-series whose label value
    /// changes, which would strand a timeseries per hand-off.
    pub fn set_fusion_frame(
        &self,
        values: &[Option<f64>; 5],
        candidate_duties: &[Option<u8>; 5],
        winner: Option<SignalKind>,
    ) {
        let mut s = self.state();
        s.signal_values.clear();
        s.signal_candidate_duty.clear();
        for &kind in SignalKind::ALL.iter() {
            if let Some(v) = values[kind.idx()] {
                s.signal_values.insert(kind.as_str(), (v, kind.unit()));
            }
            if let Some(d) = candidate_duties[kind.idx()] {
                s.signal_candidate_duty.insert(kind.as_str(), d);
            }
        }
        s.control_signal = winner.map(|w| w.as_str());
    }
    /// Enforced GPU power limit, when known. Enables percent-of-limit
    /// dashboards regardless of which unit the power curve itself uses.
    pub fn set_gpu_power_limit_watts(&self, v: f64) {
        self.state().gpu_power_limit_w = Some(v);
    }
    /// Snapshot of NVML throttle-reason bits for this tick.
    pub fn set_throttle(&self, flags: ThrottleFlags) {
        self.state().throttle = Some(flags);
    }
    /// 1 while the throttle floor is raising the applied duty above what the
    /// fused signals alone would command (including its hold window).
    pub fn set_throttle_floor_active(&self, v: bool) {
        self.state().throttle_floor_active = v;
    }
    /// A REAL read failure for this signal (not `NotSupported`, which means
    /// the signal is simply absent from this card/driver).
    pub fn inc_signal_error(&self, signal: SignalKind) {
        *self
            .state()
            .signal_errors
            .entry(signal.as_str())
            .or_insert(0) += 1;
    }
    /// A REAL failure reading NVML's throttle-reason bits. Deliberately not
    /// a `SignalKind`: throttle drives the duty floor and the alarm ladder's
    /// near-limit rung, not a fusion curve. It still needs a counter, because
    /// on a config whose only other signal is `gpu_temp` (which has no error
    /// counter of its own) a persistently failing throttle read would
    /// silently disable both safety behaviors with nothing observable above
    /// `log::debug!`.
    pub fn inc_throttle_error(&self) {
        *self.state().signal_errors.entry("throttle").or_insert(0) += 1;
    }

    /// Publishes this tick's per-card readings, REPLACING the previous
    /// frame for the same reason `set_fusion_frame` does: a card that drops
    /// off the bus (or a signal a card stops reporting) must stop emitting
    /// rather than freeze at its last value. An empty slice clears every
    /// per-card series.
    pub fn set_gpu_frame(&self, gpus: &[GpuReading]) {
        self.state().gpus = gpus.to_vec();
    }
    /// A tick on which at least one real (non-`NotSupported`) NVML read
    /// failed for this card. Survives frame replacement — it's a counter.
    pub fn inc_gpu_read_error(&self, index: u32) {
        *self.state().gpu_read_errors.entry(index).or_insert(0) += 1;
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

        // --- Per-card GPU view (strictly additive) ---
        // The families above carry the worst case across cards, which is
        // what drives the fans; these say which card it came from.
        if !s.gpus.is_empty() {
            out.push_str("# HELP unifand_gpu_device_info One series per GPU, value always 1; join on gpu for the card's name and UUID (the index is not stable across reboots, the UUID is).\n# TYPE unifand_gpu_device_info gauge\n");
            for g in &s.gpus {
                out.push_str(&format!(
                    "unifand_gpu_device_info{{gpu=\"{}\",name=\"{}\",uuid=\"{}\"}} 1\n",
                    g.index,
                    escape_label(&g.name),
                    escape_label(&g.uuid)
                ));
            }
            push_gpu_gauge(
                &mut out,
                &s.gpus,
                "unifand_gpu_device_temp_celsius",
                "Raw die temperature per GPU.",
                |g| g.temp_c,
            );
            push_gpu_gauge(
                &mut out,
                &s.gpus,
                "unifand_gpu_device_power_watts",
                "Raw power draw per GPU.",
                |g| g.power_w,
            );
            push_gpu_gauge(
                &mut out,
                &s.gpus,
                "unifand_gpu_device_power_limit_watts",
                "Enforced power limit per GPU.",
                |g| g.power_limit_w,
            );
            push_gpu_gauge(
                &mut out,
                &s.gpus,
                "unifand_gpu_device_thermal_margin_celsius",
                "Headroom to each GPU's own thermal limit.",
                |g| g.thermal_margin_c,
            );
            push_gpu_gauge(
                &mut out,
                &s.gpus,
                "unifand_gpu_device_mem_temp_celsius",
                "Memory junction temperature per GPU, where NVML exposes it.",
                |g| g.mem_temp_c,
            );
            if s.gpus.iter().any(|g| g.throttle.is_some()) {
                out.push_str("# HELP unifand_gpu_device_throttle_active NVML throttle reason currently asserted, per GPU.\n# TYPE unifand_gpu_device_throttle_active gauge\n");
                for g in &s.gpus {
                    let Some(t) = g.throttle else { continue };
                    for (reason, active) in [
                        ("sw_thermal", t.sw_thermal),
                        ("hw_thermal", t.hw_thermal),
                        ("hw_power_brake", t.hw_power_brake),
                        ("sw_power_cap", t.sw_power_cap),
                    ] {
                        out.push_str(&format!(
                            "unifand_gpu_device_throttle_active{{gpu=\"{}\",reason=\"{}\"}} {}\n",
                            g.index,
                            reason,
                            if active { 1 } else { 0 }
                        ));
                    }
                }
            }
        }
        if !s.gpu_read_errors.is_empty() {
            out.push_str("# HELP unifand_gpu_device_read_errors_total Ticks on which a real NVML read failed for this GPU; NotSupported is not counted.\n# TYPE unifand_gpu_device_read_errors_total counter\n");
            for (gpu, count) in &s.gpu_read_errors {
                out.push_str(&format!(
                    "unifand_gpu_device_read_errors_total{{gpu=\"{}\"}} {}\n",
                    gpu, count
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

    /// Builds a `set_fusion_frame` argument pair from sparse lists, so a
    /// test reads as "these signals were present on this tick" rather than
    /// as five-slot array literals.
    fn frame(
        values: &[(SignalKind, f64)],
        candidates: &[(SignalKind, u8)],
    ) -> ([Option<f64>; 5], [Option<u8>; 5]) {
        let mut v: [Option<f64>; 5] = [None; 5];
        let mut c: [Option<u8>; 5] = [None; 5];
        for &(k, x) in values {
            v[k.idx()] = Some(x);
        }
        for &(k, d) in candidates {
            c[k.idx()] = Some(d);
        }
        (v, c)
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
        let (v, c) = frame(&[(SignalKind::GpuPower, 412.5)], &[]);
        m.set_fusion_frame(&v, &c, None);
        let out = m.render();
        assert!(out.contains("# HELP unifand_signal_value"));
        assert!(out.contains("# TYPE unifand_signal_value gauge"));
        assert!(out.contains("unifand_signal_value{signal=\"gpu_power\",unit=\"watts\"} 412.5"));
    }

    #[test]
    fn candidate_duty_renders_per_signal() {
        let m = m();
        let (v, c) = frame(
            &[],
            &[(SignalKind::GpuTemp, 55), (SignalKind::GpuPower, 80)],
        );
        m.set_fusion_frame(&v, &c, None);
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
        let (v, c) = frame(
            &[],
            &[(SignalKind::GpuPower, 80), (SignalKind::GpuTemp, 55)],
        );
        m.set_fusion_frame(&v, &c, Some(SignalKind::GpuPower));
        let out = m.render();
        assert!(out.contains("# TYPE unifand_control_signal gauge"));
        assert!(out.contains("unifand_control_signal{signal=\"gpu_power\"} 1"));
        assert!(out.contains("unifand_control_signal{signal=\"gpu_temp\"} 0"));
        assert!(!out.contains("signal=\"mem_temp\""));
    }

    #[test]
    fn absent_signal_emits_no_series() {
        let m = m();
        let (v, c) = frame(
            &[(SignalKind::GpuPower, 100.0)],
            &[(SignalKind::GpuPower, 60)],
        );
        m.set_fusion_frame(&v, &c, Some(SignalKind::GpuPower));
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
    fn no_winner_clears_the_one_hot_instead_of_latching_the_last_one() {
        // Regression: set_control_signal used to be call-gated on a Some
        // winner and never cleared, so a floor-only tick (throttle floor
        // drives duty, no curve candidate wins) kept reporting the PREVIOUS
        // winner as 1 -- in exactly the degraded state a reader cares about.
        let m = m();
        let (v, c) = frame(
            &[(SignalKind::GpuPower, 500.0)],
            &[(SignalKind::GpuPower, 90), (SignalKind::GpuTemp, 40)],
        );
        m.set_fusion_frame(&v, &c, Some(SignalKind::GpuPower));
        assert!(m
            .render()
            .contains("unifand_control_signal{signal=\"gpu_power\"} 1"));

        // Next tick: same candidates, but nothing won.
        m.set_fusion_frame(&v, &c, None);
        let out = m.render();
        assert!(
            out.contains("unifand_control_signal{signal=\"gpu_power\"} 0"),
            "stale winner still latched at 1:\n{out}"
        );
        assert!(out.contains("unifand_control_signal{signal=\"gpu_temp\"} 0"));
    }

    #[test]
    fn a_signal_that_disappears_stops_emitting_series() {
        // The frame REPLACES the previous tick's view; a signal present at
        // t0 and gone at t1 must not freeze at its last value.
        let m = m();
        let (v0, c0) = frame(
            &[(SignalKind::GpuPower, 412.5), (SignalKind::GpuTemp, 71.0)],
            &[(SignalKind::GpuPower, 80), (SignalKind::GpuTemp, 55)],
        );
        m.set_fusion_frame(&v0, &c0, Some(SignalKind::GpuPower));
        assert!(m.render().contains("signal=\"gpu_power\""));

        let (v1, c1) = frame(&[(SignalKind::GpuTemp, 71.0)], &[(SignalKind::GpuTemp, 55)]);
        m.set_fusion_frame(&v1, &c1, Some(SignalKind::GpuTemp));
        let out = m.render();
        assert!(
            !out.contains("signal=\"gpu_power\""),
            "gpu_power series survived its own disappearance:\n{out}"
        );
        assert!(out.contains("unifand_signal_value{signal=\"gpu_temp\",unit=\"celsius\"} 71"));
        assert!(out.contains("unifand_control_signal{signal=\"gpu_temp\"} 1"));
    }

    #[test]
    fn throttle_read_failures_count_under_their_own_label() {
        // Throttle has no SignalKind, but it gates the duty floor and the
        // alarm's near-limit rung, so a failing read must still be visible.
        let m = m();
        m.inc_throttle_error();
        m.inc_throttle_error();
        m.inc_signal_error(SignalKind::GpuPower);
        let out = m.render();
        assert!(out.contains("# TYPE unifand_signal_errors_total counter"));
        assert!(out.contains("unifand_signal_errors_total{signal=\"throttle\"} 2"));
        assert!(out.contains("unifand_signal_errors_total{signal=\"gpu_power\"} 1"));
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
            "unifand_gpu_device_",
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

    // -- per-card GPU metrics --------------------------------------------

    fn two_cards() -> [GpuReading; 2] {
        [
            GpuReading {
                index: 0,
                name: "NVIDIA GeForce RTX 5090".to_string(),
                uuid: "GPU-aaaa".to_string(),
                temp_c: Some(71.0),
                power_w: Some(412.5),
                power_limit_w: Some(575.0),
                thermal_margin_c: Some(19.0),
                mem_temp_c: None,
                throttle: Some(ThrottleFlags {
                    sw_power_cap: true,
                    ..ThrottleFlags::default()
                }),
                read_error: false,
            },
            GpuReading {
                index: 1,
                name: "NVIDIA RTX PRO 6000".to_string(),
                uuid: "GPU-bbbb".to_string(),
                temp_c: Some(64.0),
                power_w: Some(300.0),
                power_limit_w: Some(600.0),
                thermal_margin_c: Some(26.0),
                mem_temp_c: Some(82.0),
                throttle: None,
                read_error: false,
            },
        ]
    }

    #[test]
    fn gpu_frame_renders_every_reading_per_card() {
        let m = m();
        m.set_gpu_frame(&two_cards());
        let out = m.render();
        for line in [
            "# TYPE unifand_gpu_device_temp_celsius gauge",
            "unifand_gpu_device_temp_celsius{gpu=\"0\"} 71",
            "unifand_gpu_device_temp_celsius{gpu=\"1\"} 64",
            "unifand_gpu_device_power_watts{gpu=\"0\"} 412.5",
            "unifand_gpu_device_power_watts{gpu=\"1\"} 300",
            "unifand_gpu_device_power_limit_watts{gpu=\"0\"} 575",
            "unifand_gpu_device_power_limit_watts{gpu=\"1\"} 600",
            "unifand_gpu_device_thermal_margin_celsius{gpu=\"0\"} 19",
            "unifand_gpu_device_thermal_margin_celsius{gpu=\"1\"} 26",
            "unifand_gpu_device_mem_temp_celsius{gpu=\"1\"} 82",
        ] {
            assert!(out.contains(line), "missing {line:?} in:\n{out}");
        }
    }

    #[test]
    fn gpu_info_identifies_each_card_by_name_and_uuid() {
        let m = m();
        m.set_gpu_frame(&two_cards());
        let out = m.render();
        assert!(out.contains(
            "unifand_gpu_device_info{gpu=\"0\",name=\"NVIDIA GeForce RTX 5090\",uuid=\"GPU-aaaa\"} 1"
        ));
        assert!(out.contains(
            "unifand_gpu_device_info{gpu=\"1\",name=\"NVIDIA RTX PRO 6000\",uuid=\"GPU-bbbb\"} 1"
        ));
    }

    #[test]
    fn gpu_info_escapes_label_values() {
        let m = m();
        m.set_gpu_frame(&[GpuReading {
            name: "odd \"card\" \\ name".to_string(),
            ..GpuReading::default()
        }]);
        assert!(m.render().contains("name=\"odd \\\"card\\\" \\\\ name\""));
    }

    #[test]
    fn gpu_signal_a_card_does_not_expose_emits_no_series_for_it() {
        let m = m();
        m.set_gpu_frame(&two_cards());
        let out = m.render();
        // Card 0 has no memory temperature and card 1 reports no throttle
        // reasons: absent, not zero.
        assert!(!out.contains("unifand_gpu_device_mem_temp_celsius{gpu=\"0\"}"));
        assert!(!out.contains("unifand_gpu_device_throttle_active{gpu=\"1\""));
    }

    #[test]
    fn gpu_throttle_renders_all_four_reasons_per_card() {
        let m = m();
        m.set_gpu_frame(&two_cards());
        let out = m.render();
        for line in [
            "unifand_gpu_device_throttle_active{gpu=\"0\",reason=\"sw_thermal\"} 0",
            "unifand_gpu_device_throttle_active{gpu=\"0\",reason=\"hw_thermal\"} 0",
            "unifand_gpu_device_throttle_active{gpu=\"0\",reason=\"hw_power_brake\"} 0",
            "unifand_gpu_device_throttle_active{gpu=\"0\",reason=\"sw_power_cap\"} 1",
        ] {
            assert!(out.contains(line), "missing {line:?} in:\n{out}");
        }
    }

    #[test]
    fn a_card_that_disappears_stops_emitting_series() {
        let m = m();
        let cards = two_cards();
        m.set_gpu_frame(&cards);
        assert!(m.render().contains("gpu=\"1\""));

        m.set_gpu_frame(&cards[..1]);
        let out = m.render();
        assert!(
            !out.contains("gpu=\"1\""),
            "card 1 survived its own disappearance:\n{out}"
        );
        assert!(out.contains("unifand_gpu_device_temp_celsius{gpu=\"0\"} 71"));

        m.set_gpu_frame(&[]);
        assert!(!m.render().contains("unifand_gpu_device_temp_celsius"));
    }

    #[test]
    fn gpu_read_errors_count_per_card_and_outlive_the_frame() {
        let m = m();
        m.inc_gpu_read_error(1);
        m.inc_gpu_read_error(1);
        m.inc_gpu_read_error(0);
        // A card whose reads are failing may have nothing else to publish.
        m.set_gpu_frame(&[]);
        let out = m.render();
        assert!(out.contains("# TYPE unifand_gpu_device_read_errors_total counter"));
        assert!(out.contains("unifand_gpu_device_read_errors_total{gpu=\"0\"} 1"));
        assert!(out.contains("unifand_gpu_device_read_errors_total{gpu=\"1\"} 2"));
    }
}
