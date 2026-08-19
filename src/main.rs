mod alarm;
mod config;
mod curve;
mod hid;
mod metrics;
mod rgb;
mod sensors;
mod signals;

use alarm::{AlarmInputs, AlarmMachine, LedCommand, ThermalHints};
use anyhow::Result;
use config::{Config, RgbConfig};
use curve::Controller;
use metrics::now_unix_secs;
use sensors::{
    control_temp, format_probe, should_reprobe, CpuSensor, GpuCaps, GpuSensor, MarginSource,
    POWER_LIMIT_REFRESH_TICKS, REDISCOVERY_INTERVAL_TICKS,
};
use signals::{fuse, SignalConditioner, SignalKind, SignalReadings, ThrottleLatch};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Args {
    config: PathBuf,
    oneshot: bool,
    probe_gpu: bool,
}

struct TempReadings {
    gpu: Option<f64>,
    cpu: Option<f64>,
    /// True when a *read* was attempted (sensor was Some) and failed. Never
    /// true when the sensor itself is absent — that's a separate condition
    /// (tracked by the caller via `.is_none()`) driving re-discovery.
    gpu_read_failed: bool,
    cpu_read_failed: bool,
}

/// Persistent state for the multi-signal fusion path, constructed once (only
/// when `cfg.signals.enabled`) before the control loop starts, and threaded
/// through every tick thereafter.
struct SignalsRuntime {
    conditioner: SignalConditioner,
    latch: ThrottleLatch,
    /// GPU capability probe, `None` when no GPU sensor is present. Refreshed
    /// on GPU re-discovery (`reset_gpu` + a fresh `probe_caps`).
    caps: Option<GpuCaps>,
    /// Wall-clock time of the previous tick, used to compute the elapsed
    /// seconds fed to `ThrottleLatch::update`. Independent of the alarm
    /// ladder's own `last_tick` variable so this module's timing can never
    /// perturb legacy alarm-ladder dwell accounting.
    last_tick: Instant,
    /// Ticks since the cached GPU power limit was last refreshed; compared
    /// against `POWER_LIMIT_REFRESH_TICKS` via the existing `should_reprobe`
    /// cadence helper.
    ticks_since_power_limit_probe: u32,
}

/// One human-readable `margin=...` token for the startup capability log,
/// naming the source when margin is live.
fn margin_log_token(margin: MarginSource) -> String {
    match margin {
        MarginSource::GpuMaxMinusTemp { .. } => "true(GpuMaxMinusTemp)".to_string(),
        MarginSource::TlimitField => "true(TlimitField)".to_string(),
        MarginSource::Unavailable => "false".to_string(),
    }
}

/// Builds this tick's raw `SignalReadings` from the already-read CPU/GPU
/// temperatures plus (when a GPU and its capability probe are both present)
/// a fresh NVML multi-signal read. Never touches hardware beyond the one
/// `read_signals` call; a missing GPU/caps simply yields temps-only readings.
/// Real (non-`NotSupported`) NVML read failures are folded into
/// `unifand_signal_errors_total`: power, margin and mem_temp under their own
/// `SignalKind`, and throttle-reason reads under a bare "throttle" label —
/// it has no curve-fusion `SignalKind`, but it gates the duty floor and the
/// alarm's near-limit rung, so its failures must be countable too.
fn build_signal_readings(
    gpu: &Option<GpuSensor>,
    caps: &Option<GpuCaps>,
    temps: &TempReadings,
    m: &metrics::Metrics,
) -> SignalReadings {
    let mut raw = match (gpu, caps) {
        (Some(g), Some(c)) => {
            let (readings, errors) = g.read_signals(c);
            if errors.power {
                m.inc_signal_error(SignalKind::GpuPower);
            }
            if errors.margin {
                m.inc_signal_error(SignalKind::ThermalMargin);
            }
            if errors.mem_temp {
                m.inc_signal_error(SignalKind::MemTemp);
            }
            if errors.throttle {
                m.inc_throttle_error();
            }
            readings
        }
        _ => SignalReadings::default(),
    };
    raw.gpu_temp_c = temps.gpu;
    raw.cpu_temp_c = temps.cpu;
    raw
}

fn parse_args() -> Args {
    let mut args = Args {
        config: PathBuf::from("/etc/unifand/config.toml"),
        oneshot: false,
        probe_gpu: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => {
                args.config = PathBuf::from(it.next().expect("--config requires a path"));
            }
            "--oneshot" => args.oneshot = true,
            "--probe-gpu" => args.probe_gpu = true,
            other => {
                eprintln!(
                    "unknown argument: {other}\nusage: unifand [--config PATH] [--oneshot] [--probe-gpu]"
                );
                std::process::exit(2);
            }
        }
    }
    args
}

fn read_temps(cpu: &Option<CpuSensor>, gpu: &Option<GpuSensor>) -> TempReadings {
    let mut cpu_read_failed = false;
    let cpu_t = cpu.as_ref().and_then(|s| {
        s.read_c()
            .map_err(|e| {
                log::warn!("cpu read: {e:#}");
                cpu_read_failed = true;
            })
            .ok()
    });
    let mut gpu_read_failed = false;
    let gpu_t = gpu.as_ref().and_then(|s| {
        s.read_c()
            .map_err(|e| {
                log::warn!("gpu read: {e:#}");
                gpu_read_failed = true;
            })
            .ok()
    });
    TempReadings {
        gpu: gpu_t,
        cpu: cpu_t,
        gpu_read_failed,
        cpu_read_failed,
    }
}

/// Write `duty` to every channel; returns true iff all writes succeeded.
fn apply_duty(hub: &mut hid::Hub, channels: &[u8], duty: u8, m: &metrics::Metrics) -> bool {
    let mut ok = true;
    for &c in channels {
        if let Err(e) = hub.set_duty(c, duty) {
            log::warn!("set_duty ch{c}: {e:#}");
            m.inc_hid_errors_write();
            ok = false;
        } else {
            m.set_duty(c, duty);
        }
    }
    ok
}

/// Write `color`/`brightness` to every RGB channel; returns true iff all
/// writes succeeded. RGB failures are cosmetic only — callers must never let
/// this affect `consecutive_errors` or hub re-enumeration.
fn apply_rgb(
    hub: &mut hid::Hub,
    channels: &[u8],
    rgb_cfg: &RgbConfig,
    color: (u8, u8, u8),
    brightness: u8,
    m: &metrics::Metrics,
) -> bool {
    let mut ok = true;
    for &c in channels {
        let num_fans = rgb_cfg.fans_for_channel(c);
        if let Err(e) = hub.set_rgb(c, num_fans, color, brightness) {
            log::warn!("set_rgb ch{c}: {e:#}");
            m.inc_rgb_errors();
            ok = false;
        }
    }
    ok
}

/// Write an alarm-ladder `LedCommand` to every RGB channel, diffed against
/// `prev`. When only `speed_idx` differs on an otherwise-identical Breathing
/// command, a single `set_effect_speed` commit-only write suffices; any other
/// change (color, mode, or first write) needs the full `set_effect` sequence.
/// RGB failures are cosmetic only — same contract as `apply_rgb`: warn +
/// `inc_rgb_errors`, never touch `consecutive_errors`/hub-recovery/shutdown.
fn apply_led_command(
    hub: &mut hid::Hub,
    channels: &[u8],
    rgb_cfg: &RgbConfig,
    prev: Option<LedCommand>,
    cmd: LedCommand,
    brightness: u8,
    m: &metrics::Metrics,
) -> bool {
    let speed_only = matches!(
        (prev, cmd),
        (
            Some(LedCommand::Breathing { color: pc, .. }),
            LedCommand::Breathing { color: cc, .. }
        ) if pc == cc
    );
    let mut ok = true;
    for &c in channels {
        let num_fans = rgb_cfg.fans_for_channel(c);
        let result = if speed_only {
            let LedCommand::Breathing { speed_idx, .. } = cmd else {
                unreachable!("speed_only implies Breathing");
            };
            hub.set_effect_speed(
                c,
                rgb::MODE_BREATHING,
                rgb::SPEEDS[speed_idx as usize],
                brightness,
            )
        } else {
            match cmd {
                LedCommand::Breathing { color, speed_idx } => hub.set_effect(
                    c,
                    num_fans,
                    &[color],
                    rgb::MODE_BREATHING,
                    rgb::SPEEDS[speed_idx as usize],
                    brightness,
                ),
                LedCommand::Runway { colors, speed_idx } => hub.set_effect(
                    c,
                    num_fans,
                    &colors,
                    rgb::MODE_RUNWAY,
                    rgb::SPEEDS[speed_idx as usize],
                    brightness,
                ),
            }
        };
        if let Err(e) = result {
            log::warn!("set led ch{c}: {e:#}");
            m.inc_rgb_errors();
            ok = false;
        }
    }
    ok
}

/// Sleep for `dur` in short slices so SIGTERM/SIGINT is honored promptly.
/// Returns false if termination was requested before the full duration
/// elapsed (callers should stop what they're doing and unwind).
fn sleep_interruptible(dur: Duration, term: &AtomicBool) -> bool {
    let mut remaining = dur;
    while remaining > Duration::ZERO && !term.load(Ordering::Relaxed) {
        let slice = remaining.min(Duration::from_millis(200));
        std::thread::sleep(slice);
        remaining -= slice;
    }
    !term.load(Ordering::Relaxed)
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();
    let cfg = Config::load(&args.config)?;
    log::info!(
        "loaded config from {} ({} curve points, channels {:?})",
        args.config.display(),
        cfg.curve.len(),
        cfg.channels
    );
    if cfg.rgb.alerts.enabled && !cfg.rgb.enabled {
        log::warn!(
            "rgb.alerts.enabled=true but rgb.enabled=false; the alarm ladder \
             is inert (it is only constructed when rgb.enabled=true) — no \
             state tracking, no LEDs, no escalation. Set rgb.enabled=true to \
             activate it"
        );
    }

    let mut cpu = CpuSensor::discover();
    let mut gpu = GpuSensor::init();
    log::info!(
        "sensors: cpu(k10temp)={} gpu(nvml)={}",
        cpu.is_some(),
        gpu.is_some()
    );

    if args.probe_gpu {
        return match &gpu {
            Some(g) => {
                print!("{}", format_probe(&g.probe_snapshot()));
                Ok(())
            }
            None => {
                println!("gpu sensor absent: no NVML device found");
                Ok(())
            }
        };
    }

    if args.oneshot {
        let temps = read_temps(&cpu, &gpu);
        let (ctrl, degraded) = control_temp(temps.gpu, temps.cpu, cfg.cpu_offset);
        let duty = ctrl.map(|t| curve::interpolate(&cfg.curve, t));
        println!("gpu={:?} cpu={:?} control={ctrl:?} degraded={degraded} -> duty={duty:?} (hub untouched)", temps.gpu, temps.cpu);

        // Fusion preview: read-only, touches no hardware beyond the NVML
        // reads `read_temps`/`GpuSensor::probe_caps`/`read_signals` already
        // perform above (never the hub). Uses a fresh, one-shot
        // conditioner/throttle-state — there is no prior tick to hold state
        // across in a single-shot invocation.
        if cfg.signals.enabled {
            let caps = gpu.as_ref().map(|g| g.probe_caps());
            let mut raw = match (&gpu, &caps) {
                (Some(g), Some(c)) => g.read_signals(c).0,
                _ => SignalReadings::default(),
            };
            raw.gpu_temp_c = temps.gpu;
            raw.cpu_temp_c = temps.cpu;

            let mut conditioner = SignalConditioner::new(&cfg.signals);
            let cond = conditioner.update(&raw);
            let throttle_active = raw.throttle.any_of(&cfg.signals.throttle.reasons);
            let fusion = fuse(&cfg.signals, &cfg.curve, &cond, throttle_active);

            for candidate in fusion.candidates.iter().flatten() {
                println!(
                    "signal={} value={:.2} candidate_duty={}",
                    candidate.kind.as_str(),
                    candidate.value,
                    candidate.duty
                );
            }
            println!(
                "fusion winner={:?} target={:?} floor_applied={}",
                fusion.winner, fusion.target, fusion.floor_applied
            );
        }
        return Ok(());
    }

    // Start the metrics server before the hub is found: /healthz's grace
    // window (seeded to process-start time) and unifand_hub_present both
    // need to be servable while we're still waiting on the hub below.
    let m = metrics::Metrics::new(cfg.poll_interval_secs);
    metrics::serve(m.clone(), cfg.metrics.listen.clone());

    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, term.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, term.clone())?;

    // Hub-absent at startup is no longer fatal (a hard-fail here used to
    // crashloop with kubelet's 5-minute backoff cliff on late USB
    // enumeration/hub brown-out). Retry every 5s, warning at most every 30s,
    // until either the hub is found+initialized or shutdown is requested.
    let mut last_hub_warn = Instant::now() - Duration::from_secs(30);
    let mut hub = loop {
        let attempt = hid::find_hidraw().and_then(|p| {
            let mut h = hid::Hub::open(&p)?;
            h.init(&cfg.channels)?;
            Ok((p, h))
        });
        match attempt {
            Ok((p, h)) => {
                log::info!("hub at {}", p.display());
                m.set_hub_present(true);
                break h;
            }
            Err(e) => {
                m.set_hub_present(false);
                // Keep last_tick fresh while waiting so /healthz's liveness
                // grace doesn't expire and get the pod killed mid-wait — a
                // liveness kill here would just restart into the same wait.
                m.set_last_tick(now_unix_secs());
                if last_hub_warn.elapsed() >= Duration::from_secs(30) {
                    log::warn!("hub not found yet, retrying every 5s: {e:#}");
                    last_hub_warn = Instant::now();
                }
                // Refresh last_tick at least once per second while waiting
                // out the 5s retry cadence, not just once per attempt: the
                // healthz grace window is 3 * poll_interval_secs, and any
                // poll_interval_secs below ~1.7s (a valid config — only a
                // zero floor is enforced) would otherwise let /healthz go
                // stale-500 mid-wait even though the daemon is retrying on
                // schedule. Sliced via the same sleep_interruptible used
                // elsewhere so SIGTERM/SIGINT is still honored promptly.
                let retry_after = Duration::from_secs(5);
                let mut waited = Duration::ZERO;
                while waited < retry_after {
                    let slice = (retry_after - waited).min(Duration::from_secs(1));
                    if !sleep_interruptible(slice, &term) {
                        log::info!("shutdown requested while waiting for hub");
                        return Ok(());
                    }
                    waited += slice;
                    m.set_last_tick(now_unix_secs());
                }
            }
        }
    };

    // Multi-signal fusion runtime: constructed once, only when
    // cfg.signals.enabled, and threaded through every tick thereafter. The
    // ONE startup log line below names which GPU-derived signals are
    // actually live on this hardware, from the same capability probe the
    // per-tick reads use.
    let mut signals_rt: Option<SignalsRuntime> = if cfg.signals.enabled {
        let caps = gpu.as_ref().map(|g| g.probe_caps());
        let (power, margin, mem_temp, throttle) = match &caps {
            Some(c) => (c.power, margin_log_token(c.margin), c.mem_temp, c.throttle),
            None => (
                false,
                margin_log_token(MarginSource::Unavailable),
                false,
                false,
            ),
        };
        log::info!(
            "signals: power={power} margin={margin} mem_temp={mem_temp} throttle={throttle}"
        );
        Some(SignalsRuntime {
            conditioner: SignalConditioner::new(&cfg.signals),
            latch: ThrottleLatch::new(cfg.signals.throttle.hold_secs),
            caps,
            last_tick: Instant::now(),
            ticks_since_power_limit_probe: 0,
        })
    } else {
        None
    };

    let mut ctl = Controller::new(cfg.hysteresis_c, cfg.min_duty_delta, cfg.max_step_per_tick);
    let mut consecutive_errors: u32 = 0;
    // Last duty the control loop decided to apply (regardless of whether the
    // write succeeded). Used to re-assert state after a hub re-init, since
    // hysteresis can otherwise suppress decide() from ever returning
    // Some(duty) again once the ramp reaches target.
    let mut last_applied: Option<u8> = None;
    // Duty bucket the RGB colors currently on the hub correspond to. None
    // forces a re-assert (e.g. right after a hub re-init, since the fresh
    // hub has lost whatever color state it had before the reset).
    let mut last_bucket: Option<u8> = None;

    // Alarm ladder: pure state machine, built only when RGB is enabled (it
    // stays inert internally when cfg.rgb.alerts.enabled is false, so this is
    // the only gate needed). curve is non-empty per Config::validate().
    let mut alarm_machine = cfg
        .rgb
        .enabled
        .then(|| AlarmMachine::new(&cfg.rgb.alerts, cfg.curve.last().unwrap().temp));
    // Last LedCommand actually written to the hub. None means either no
    // custom alarm command is active (Normal) or nothing has been written
    // yet; distinguishes "back to Normal" transitions from steady state.
    let mut last_led: Option<LedCommand> = None;
    // Wall-clock time of the previous tick() call; elapsed is computed from
    // this rather than assumed to be poll_interval_secs, so a slow loop
    // iteration (retries, HID hiccups) doesn't understate dwell time.
    let mut last_tick = Instant::now();

    // Sensor re-discovery bookkeeping: a sensor that's absent, or whose reads
    // have failed 3+ ticks in a row, is retried at most every
    // REDISCOVERY_INTERVAL_TICKS (~60s at the default 5s poll interval).
    // Converts "restart the pod after a driver upgrade or late GPU-toolkit
    // injection" from tribal knowledge into a non-event.
    let mut cpu_fail_streak: u32 = 0;
    let mut gpu_fail_streak: u32 = 0;
    let mut ticks_since_cpu_probe: u32 = 0;
    let mut ticks_since_gpu_probe: u32 = 0;

    while !term.load(Ordering::Relaxed) {
        let temps = read_temps(&cpu, &gpu);
        if let Some(t) = temps.gpu {
            m.set_temp("gpu", t);
        }
        if let Some(t) = temps.cpu {
            m.set_temp("cpu", t);
        }
        if cpu.is_some() {
            cpu_fail_streak = if temps.cpu_read_failed {
                cpu_fail_streak + 1
            } else {
                0
            };
        }
        if gpu.is_some() {
            gpu_fail_streak = if temps.gpu_read_failed {
                gpu_fail_streak + 1
            } else {
                0
            };
        }
        let (ctrl_t, degraded) = control_temp(temps.gpu, temps.cpu, cfg.cpu_offset);
        m.set_degraded(degraded);

        // THE ONE INVIOLABLE RULE: when `[signals]` is absent or disabled,
        // behavior is EXACTLY v0.5.1 — the `else` arm below is that code,
        // copied verbatim (see docs/superpowers/plans/wave-7-report.md for
        // the diff-walkthrough proving it).
        let mut fallback_active = false;
        let (decision, thermal_hints): (Option<u8>, ThermalHints) = if cfg.signals.enabled {
            let sr = signals_rt
                .as_mut()
                .expect("signals_rt is constructed whenever cfg.signals.enabled");

            // Periodic refresh of the cached enforced power limit — an
            // `nvidia-smi -pl` change mid-run must not silently rescale a
            // percent_tdp curve forever. Reuses the existing `should_reprobe`
            // cadence helper with `missing_or_failing: true` since this
            // refresh is unconditionally periodic, not gated on absence.
            if should_reprobe(
                true,
                sr.ticks_since_power_limit_probe,
                POWER_LIMIT_REFRESH_TICKS,
            ) {
                sr.ticks_since_power_limit_probe = 0;
                if let (Some(g), Some(caps)) = (gpu.as_ref(), sr.caps.as_mut()) {
                    g.refresh_power_limit(caps);
                }
            } else {
                sr.ticks_since_power_limit_probe += 1;
            }

            let raw = build_signal_readings(&gpu, &sr.caps, &temps, &m);
            let cond = sr.conditioner.update(&raw);

            let sig_now = Instant::now();
            let sig_elapsed = sig_now.duration_since(sr.last_tick).as_secs();
            sr.last_tick = sig_now;
            let throttle_active = raw.throttle.any_of(&cfg.signals.throttle.reasons);
            let latched = sr.latch.update(throttle_active, sig_elapsed);

            let fusion = fuse(&cfg.signals, &cfg.curve, &cond, latched);

            // Metrics hand-off (Wave 5): publish the whole frame at once —
            // every candidate fuse() produced (winners AND non-winners, since
            // unifand_control_signal's one-hot is keyed off them) plus the
            // winner, which is None on a floor-only tick and must clear the
            // one-hot rather than leave the last winner latched at 1.
            let mut candidate_duties: [Option<u8>; 5] = [None; 5];
            for candidate in fusion.candidates.iter().flatten() {
                candidate_duties[candidate.kind.idx()] = Some(candidate.duty);
            }
            m.set_fusion_frame(&cond.values, &candidate_duties, fusion.winner);
            if let Some(limit) = cond.gpu_power_limit_w {
                m.set_gpu_power_limit_watts(limit);
            }
            m.set_throttle(cond.throttle);
            m.set_throttle_floor_active(fusion.floor_applied);

            let fusion_decision = match fusion.target {
                Some(target) => ctl.decide_target(target),
                None => {
                    // Same fallback path the legacy arm takes: no curve
                    // signal AND no throttle floor produced a target at all.
                    log::error!(
                        "all signals unavailable; applying fallback duty {}%",
                        cfg.fallback_duty
                    );
                    fallback_active = true;
                    ctl.force(cfg.fallback_duty);
                    Some(cfg.fallback_duty)
                }
            };

            // unifand_degraded keeps its exact current meaning (GPU
            // *temperature* missing) via control_temp, called here too even
            // though fusion doesn't use it to pick duty.
            if let Some(t) = ctrl_t {
                m.set_temp("control", t);
            }

            let hints = ThermalHints {
                thermal_margin_c: cond.values[SignalKind::ThermalMargin.idx()],
                thermal_throttling: latched,
            };

            (fusion_decision, hints)
        } else {
            (
                match ctrl_t {
                    Some(t) => {
                        m.set_temp("control", t);
                        ctl.decide(&cfg.curve, t)
                    }
                    None => {
                        log::error!(
                            "all sensors unavailable; applying fallback duty {}%",
                            cfg.fallback_duty
                        );
                        fallback_active = true;
                        ctl.force(cfg.fallback_duty);
                        Some(cfg.fallback_duty)
                    }
                },
                ThermalHints::default(),
            )
        };
        m.set_fallback_active(fallback_active);

        // Whether this tick's apply_duty call (if any) failed. Only this —
        // not a None decision — suppresses LED dispatch below: the hub is
        // likely unhealthy right after a failed write, so piling more writes
        // onto it this tick isn't worthwhile. A None decision is the normal
        // steady-state case (hysteresis hold, or unconditionally once
        // duty_gap==0 — e.g. pinned at 100% under sustained load) and must
        // NOT suppress LED dispatch, or alarm-ladder LED updates (notably
        // NearLimit's speed escalation) would never reach the hub while the
        // system sits at its limit.
        let mut duty_write_failed = false;
        if let Some(duty) = decision {
            last_applied = Some(duty);
            if apply_duty(&mut hub, &cfg.channels, duty, &m) {
                consecutive_errors = 0;
                log::info!("duty -> {duty}% (control {:.1?}C)", ctrl_t);
            } else {
                consecutive_errors += 1;
                duty_write_failed = true;
            }
        }

        // LED dispatch runs EVERY tick (not just ticks where decide()
        // returned Some), using last_applied.unwrap_or(0) as the duty for
        // gradient/bucket computation — the same value the AlarmInputs
        // construction below uses. The last_led/last_bucket diffing below
        // keeps steady-state ticks write-free, so running this unconditionally
        // is cheap.
        if cfg.rgb.enabled && !duty_write_failed {
            let duty = last_applied.unwrap_or(0);
            // Dispatches on the alarm state as of the END of the
            // PREVIOUS tick's machine.tick() call (advanced below,
            // after this section, using this tick's fresh readings) —
            // so a state transition takes effect on the LED one tick
            // after the ladder itself flips, same lag the bucket path
            // already had relative to duty decisions.
            match alarm_machine.as_ref().and_then(|am| am.led()) {
                None => {
                    if last_led.is_some() {
                        // Back to Normal: drop the alarm command and
                        // force the static gradient to re-assert.
                        last_led = None;
                        last_bucket = None;
                    }
                    let b = rgb::bucket(duty, cfg.rgb.buckets);
                    if last_bucket != Some(b) {
                        let color = rgb::duty_color(&cfg.rgb.stops, duty);
                        if apply_rgb(
                            &mut hub,
                            &cfg.channels,
                            &cfg.rgb,
                            color,
                            cfg.rgb.brightness,
                            &m,
                        ) {
                            last_bucket = Some(b);
                        }
                        // On failure last_bucket is left unchanged, so the
                        // next tick with a differing bucket retries.
                    }
                }
                Some(mut cmd) => {
                    // SustainedHot reports its state via a Breathing
                    // command at speed_idx 0; substitute a bucket-quantized
                    // gradient color for cfg.alert_color before
                    // diffing/writing. Quantizing through the same bucket
                    // mechanism the Normal path uses means small duty moves
                    // within a bucket don't change the substituted color, so
                    // this doesn't restart the breathing phase with a full
                    // re-send more often than the Normal path would write.
                    if let LedCommand::Breathing { speed_idx: 0, .. } = cmd {
                        let b = rgb::bucket(duty, cfg.rgb.buckets);
                        let rep_duty = rgb::bucket_duty(b, cfg.rgb.buckets);
                        let color = rgb::duty_color(&cfg.rgb.stops, rep_duty);
                        cmd = LedCommand::Breathing {
                            color,
                            speed_idx: 0,
                        };
                    }
                    // On failure last_led is left unchanged, so the
                    // next tick with a differing command retries.
                    if last_led != Some(cmd)
                        && apply_led_command(
                            &mut hub,
                            &cfg.channels,
                            &cfg.rgb,
                            last_led,
                            cmd,
                            cfg.rgb.brightness,
                            &m,
                        )
                    {
                        last_led = Some(cmd);
                    }
                }
            }
        }

        // Advance the alarm ladder for next tick, using this tick's fresh
        // control temp/duty. elapsed is wall-clock time since the previous
        // call, not the nominal poll interval, so it stays correct across
        // retries/HID hiccups. Runs every loop iteration regardless of write
        // outcome above so dwell timers never silently stall.
        if let Some(machine) = alarm_machine.as_mut() {
            let now = Instant::now();
            let elapsed = now.duration_since(last_tick).as_secs();
            last_tick = now;
            let inputs = AlarmInputs {
                control_temp: ctrl_t,
                duty: last_applied.unwrap_or(0),
                fallback_active,
            };
            if let Some(name) = machine.tick_with(&inputs, &thermal_hints, elapsed) {
                log::info!("alarm ladder state -> {name}");
            }
        }
        m.set_led_state(alarm_machine.as_ref().map_or(0, |am| am.state_code()));

        let mut rpm_read_failed = false;
        for &c in &cfg.channels {
            match hub.read_rpm(c) {
                Ok(rpm) => m.set_rpm(c, rpm),
                Err(e) => {
                    log::warn!("rpm ch{c}: {e:#}");
                    m.inc_hid_errors_read();
                    rpm_read_failed = true;
                }
            }
        }
        // Count at most once per tick, mirroring the write-failure accounting
        // above, so N failing channels don't multiply-count.
        if rpm_read_failed {
            consecutive_errors += 1;
        }

        // USB hiccup recovery: re-enumerate and re-init after repeated failures.
        if consecutive_errors >= 3 {
            log::warn!("{consecutive_errors} consecutive HID errors; re-enumerating hub");
            match hid::find_hidraw() {
                Ok(p) => match hid::Hub::open(&p) {
                    Ok(mut h) => match h.init(&cfg.channels) {
                        Ok(()) => {
                            hub = h;
                            m.set_hub_present(true);
                            log::info!("hub re-initialized at {}", p.display());
                            // Fresh hub has lost whatever LED state it had
                            // before the reset; force colors to re-assert on
                            // the next applied duty, whether that's the static
                            // gradient (last_bucket) or an active alarm command
                            // (last_led).
                            last_bucket = None;
                            last_led = None;
                            // init() only sets mode bytes, not duty; the fresh
                            // hub has lost whatever duty state it had before the
                            // reset, so the daemon's last decided duty must be
                            // rewritten now or the daemon and hub silently
                            // diverge until the next real ramp step (which
                            // hysteresis can suppress indefinitely once the ramp
                            // is already at target).
                            match last_applied {
                                Some(duty) if apply_duty(&mut hub, &cfg.channels, duty, &m) => {
                                    consecutive_errors = 0;
                                    log::info!("duty {duty}% reapplied after re-init");
                                }
                                Some(_) => {
                                    consecutive_errors += 1;
                                }
                                None => {
                                    consecutive_errors = 0;
                                }
                            }
                        }
                        Err(e) => {
                            // Opened but failed to init: the old `hub` (if
                            // any) is presumed equally wedged, so this is not
                            // "present" from a control standpoint either.
                            m.set_hub_present(false);
                            log::warn!("hub re-init failed at {}: {e:#}", p.display());
                        }
                    },
                    Err(e) => {
                        m.set_hub_present(false);
                        log::warn!("hub open failed at {}: {e:#}", p.display());
                    }
                },
                Err(e) => {
                    m.set_hub_present(false);
                    log::warn!("hub still not found during re-enumeration: {e:#}");
                }
            }
        }

        // Periodic sensor re-discovery: a sensor that's absent (never
        // discovered, or lost when a driver/toolkit restarted mid-run) or
        // whose reads have failed 3+ ticks in a row is retried at most every
        // REDISCOVERY_INTERVAL_TICKS ticks. Cheap (a directory scan / one
        // dlopen) relative to the poll interval, so this runs unconditionally.
        if should_reprobe(
            cpu.is_none() || cpu_fail_streak >= 3,
            ticks_since_cpu_probe,
            REDISCOVERY_INTERVAL_TICKS,
        ) {
            ticks_since_cpu_probe = 0;
            if let Some(s) = CpuSensor::discover() {
                log::info!("cpu sensor (re)discovered");
                cpu = Some(s);
                cpu_fail_streak = 0;
            }
        } else {
            ticks_since_cpu_probe += 1;
        }
        if should_reprobe(
            gpu.is_none() || gpu_fail_streak >= 3,
            ticks_since_gpu_probe,
            REDISCOVERY_INTERVAL_TICKS,
        ) {
            ticks_since_gpu_probe = 0;
            if let Some(s) = GpuSensor::init() {
                log::info!("gpu sensor (re)discovered");
                gpu = Some(s);
                gpu_fail_streak = 0;
                // Driver restarted / capability appeared mid-run: re-probe
                // capabilities and reset the GPU-derived filters so stale
                // conditioner state from the old NVML handle isn't blended
                // with readings from the fresh one.
                if let Some(sr) = signals_rt.as_mut() {
                    if let Some(g) = gpu.as_ref() {
                        sr.caps = Some(g.probe_caps());
                    }
                    sr.conditioner.reset_gpu();
                }
            }
        } else {
            ticks_since_gpu_probe += 1;
        }

        // Heartbeat: set at the end of every completed tick, regardless of
        // write/read outcomes above. This is what /healthz and
        // unifand_last_tick_timestamp_seconds alert on — a wedged loop stops
        // advancing this even though the metrics server keeps answering
        // requests from its own thread.
        m.set_last_tick(now_unix_secs());

        sleep_interruptible(Duration::from_secs(cfg.poll_interval_secs), &term);
    }

    log::info!(
        "shutting down: setting fallback duty {}%",
        cfg.fallback_duty
    );
    for &c in &cfg.channels {
        if let Err(e) = hub.set_duty(c, cfg.fallback_duty) {
            log::error!("fallback write ch{c} failed: {e:#}");
        }
    }
    Ok(())
}
