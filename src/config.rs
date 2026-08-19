use crate::rgb::ColorStop;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct CurvePoint {
    /// `value` is accepted as an alias so watts/headroom curves under
    /// `[signals.*]` read naturally (`value = 300`) without renaming this
    /// field — both keys are known to serde, so `serde_ignored` emits no
    /// spurious unknown-key warning for either spelling.
    #[serde(alias = "value")]
    pub temp: f64,
    pub duty: u8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlertsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_sustained_hot_c")]
    pub sustained_hot_c: f64,
    #[serde(default = "d_sustained_after_secs")]
    pub sustained_after_secs: u64,
    #[serde(default = "d_near_limit_margin_c")]
    pub near_limit_margin_c: f64,
    #[serde(default = "d_escalate_every_secs")]
    pub escalate_every_secs: u64,
    #[serde(default = "d_cooldown_secs")]
    pub cooldown_secs: u64,
    #[serde(default = "d_alert_color")]
    pub alert_color: [u8; 3],
    #[serde(default = "d_fault_colors")]
    pub fault_colors: [[u8; 3]; 2],
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sustained_hot_c: d_sustained_hot_c(),
            sustained_after_secs: d_sustained_after_secs(),
            near_limit_margin_c: d_near_limit_margin_c(),
            escalate_every_secs: d_escalate_every_secs(),
            cooldown_secs: d_cooldown_secs(),
            alert_color: d_alert_color(),
            fault_colors: d_fault_colors(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "d_listen")]
    pub listen: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { listen: d_listen() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RgbConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_rgb_fans")]
    pub fans_per_channel: u8,
    /// Per-channel override of the LED chain length declared to the hub
    /// (byte 3 of the RGB start packet: `(channel<<4)+num_fans`), keyed by
    /// channel number as a string (TOML table keys are strings). Falls back
    /// to `fans_per_channel` for any channel not listed here. Use
    /// `fans_for_channel()` rather than reading this map directly.
    #[serde(default)]
    pub fans: BTreeMap<String, u8>,
    #[serde(default = "d_rgb_brightness")]
    pub brightness: u8,
    #[serde(default = "d_rgb_buckets")]
    pub buckets: u8,
    #[serde(default = "d_rgb_stops")]
    pub stops: Vec<ColorStop>,
    #[serde(default)]
    pub alerts: AlertsConfig,
}

impl Default for RgbConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fans_per_channel: d_rgb_fans(),
            fans: BTreeMap::new(),
            brightness: d_rgb_brightness(),
            buckets: d_rgb_buckets(),
            stops: d_rgb_stops(),
            alerts: AlertsConfig::default(),
        }
    }
}

impl RgbConfig {
    /// Effective LED chain length declared to the hub for `channel`: the
    /// per-channel override in `fans` if present, else `fans_per_channel`.
    /// This is the fix for the chain-length bug — the hub only lights the
    /// number of fans declared here, so a channel with fewer/more physical
    /// fans than the global default needs its own entry.
    pub fn fans_for_channel(&self, channel: u8) -> u8 {
        self.fans
            .get(&channel.to_string())
            .copied()
            .unwrap_or(self.fans_per_channel)
    }
}

/// Config-facing enum naming a throttle reason, for the set of reasons that
/// should latch the throttle floor (`[signals.throttle] reasons = [...]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThrottleReason {
    SwThermal,
    HwThermal,
    HwPowerBrake,
    SwPowerCap,
}

/// Unit the configured `[signals.power]` curve's X axis is expressed in.
/// Default is `watts` — a portable `percent_tdp` default risks silently
/// reinterpreting a lab-tuned watts curve, so the explicit default favors
/// the less surprising failure mode (see `validate`'s `percent_tdp` guard).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerUnit {
    #[default]
    Watts,
    PercentTdp,
}

/// GPU die temperature signal. Enabled by default; falls back to the
/// top-level `[[curve]]` when its own `curve` is left empty (Wave 1
/// `fuse()`'s contract — see `src/signals.rs`).
#[derive(Debug, Clone, Deserialize)]
pub struct GpuTempConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default)]
    pub curve: Vec<CurvePoint>,
    #[serde(default = "d_signal_alpha_one")]
    pub alpha: f64,
}

impl Default for GpuTempConfig {
    fn default() -> Self {
        Self {
            enabled: d_true(),
            curve: Vec::new(),
            alpha: d_signal_alpha_one(),
        }
    }
}

/// CPU temperature signal. Enabled by default; `offset_c` reuses the same
/// default as the legacy top-level `cpu_offset`. Falls back to the
/// top-level `[[curve]]` when its own `curve` is left empty.
#[derive(Debug, Clone, Deserialize)]
pub struct CpuTempConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default)]
    pub curve: Vec<CurvePoint>,
    #[serde(default = "d_cpu_offset")]
    pub offset_c: f64,
    #[serde(default = "d_signal_alpha_one")]
    pub alpha: f64,
}

impl Default for CpuTempConfig {
    fn default() -> Self {
        Self {
            enabled: d_true(),
            curve: Vec::new(),
            offset_c: d_cpu_offset(),
            alpha: d_signal_alpha_one(),
        }
    }
}

/// GPU power signal (`[signals.power]`). Disabled by default; unlike
/// gpu_temp/cpu_temp there is no top-level fallback for a watts/percent
/// curve, so `curve` is required by `validate` when enabled.
#[derive(Debug, Clone, Deserialize)]
pub struct GpuPowerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub curve: Vec<CurvePoint>,
    #[serde(default = "d_power_unit")]
    pub unit: PowerUnit,
    #[serde(default = "d_power_rise_alpha")]
    pub rise_alpha: f64,
    #[serde(default = "d_power_fall_alpha")]
    pub fall_alpha: f64,
}

impl Default for GpuPowerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            curve: Vec::new(),
            unit: d_power_unit(),
            rise_alpha: d_power_rise_alpha(),
            fall_alpha: d_power_fall_alpha(),
        }
    }
}

/// Thermal margin signal (`[signals.margin]`, headroom-to-limit °C).
/// Disabled by default; `curve` is required by `validate` when enabled (no
/// top-level fallback — margin's X axis isn't a die temperature).
#[derive(Debug, Clone, Deserialize)]
pub struct MarginConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub curve: Vec<CurvePoint>,
    #[serde(default = "d_margin_alpha")]
    pub alpha: f64,
}

impl Default for MarginConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            curve: Vec::new(),
            alpha: d_margin_alpha(),
        }
    }
}

/// Memory temperature signal (`[signals.mem_temp]`). Disabled by default;
/// `curve` is required by `validate` when enabled.
#[derive(Debug, Clone, Deserialize)]
pub struct MemTempConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub curve: Vec<CurvePoint>,
    #[serde(default = "d_signal_alpha_one")]
    pub alpha: f64,
}

impl Default for MemTempConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            curve: Vec::new(),
            alpha: d_signal_alpha_one(),
        }
    }
}

/// Throttle-reason duty floor (`[signals.throttle]`) — anti-flap safety net,
/// not a routine control path (Wave 0 hardware data: no thermal throttle bit
/// ever asserted, even at 84 °C with 5 °C headroom).
#[derive(Debug, Clone, Deserialize)]
pub struct ThrottleConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_throttle_floor_duty")]
    pub floor_duty: u8,
    #[serde(default = "d_throttle_hold_secs")]
    pub hold_secs: u64,
    #[serde(default = "d_throttle_reasons")]
    pub reasons: Vec<ThrottleReason>,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            floor_duty: d_throttle_floor_duty(),
            hold_secs: d_throttle_hold_secs(),
            reasons: d_throttle_reasons(),
        }
    }
}

/// `[signals]` section: multi-signal fan-curve fusion (GPU power, thermal
/// margin, throttle floor), gated behind `enabled` — inert unless turned on,
/// mirroring `[rgb]`.
#[derive(Debug, Clone, Deserialize)]
pub struct SignalsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Applied uniformly to all 5 signal filters in `SignalConditioner`.
    /// Defaults (rise 0.5 / fall 0.1 at a 5 s tick) give τ_rise ≈ 7 s,
    /// τ_fall ≈ 47 s.
    #[serde(default = "d_signals_rise_alpha")]
    pub rise_alpha: f64,
    #[serde(default = "d_signals_fall_alpha")]
    pub fall_alpha: f64,
    #[serde(default)]
    pub gpu_temp: GpuTempConfig,
    #[serde(default)]
    pub cpu_temp: CpuTempConfig,
    #[serde(default, rename = "power")]
    pub gpu_power: GpuPowerConfig,
    #[serde(default, rename = "margin")]
    pub thermal_margin: MarginConfig,
    #[serde(default)]
    pub mem_temp: MemTempConfig,
    #[serde(default)]
    pub throttle: ThrottleConfig,
}

impl Default for SignalsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rise_alpha: d_signals_rise_alpha(),
            fall_alpha: d_signals_fall_alpha(),
            gpu_temp: GpuTempConfig::default(),
            cpu_temp: CpuTempConfig::default(),
            gpu_power: GpuPowerConfig::default(),
            thermal_margin: MarginConfig::default(),
            mem_temp: MemTempConfig::default(),
            throttle: ThrottleConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "d_poll")]
    pub poll_interval_secs: u64,
    #[serde(default = "d_channels")]
    pub channels: Vec<u8>,
    #[serde(default = "d_fallback")]
    pub fallback_duty: u8,
    #[serde(default = "d_cpu_offset")]
    pub cpu_offset: f64,
    #[serde(default = "d_hysteresis")]
    pub hysteresis_c: f64,
    #[serde(default = "d_min_delta")]
    pub min_duty_delta: u8,
    #[serde(default = "d_max_step")]
    pub max_step_per_tick: u8,
    #[serde(default)]
    pub curve: Vec<CurvePoint>,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub rgb: RgbConfig,
    #[serde(default)]
    pub signals: SignalsConfig,
}

fn d_sustained_hot_c() -> f64 {
    75.0
}
fn d_sustained_after_secs() -> u64 {
    120
}
fn d_near_limit_margin_c() -> f64 {
    3.0
}
fn d_escalate_every_secs() -> u64 {
    30
}
fn d_cooldown_secs() -> u64 {
    60
}
fn d_alert_color() -> [u8; 3] {
    [255, 0, 0]
}
fn d_fault_colors() -> [[u8; 3]; 2] {
    [[255, 80, 0], [255, 0, 0]]
}

fn d_poll() -> u64 {
    5
}
fn d_channels() -> Vec<u8> {
    vec![1, 2]
}
fn d_fallback() -> u8 {
    60
}
fn d_cpu_offset() -> f64 {
    10.0
}
fn d_hysteresis() -> f64 {
    2.0
}
fn d_min_delta() -> u8 {
    5
}
fn d_max_step() -> u8 {
    10
}
fn d_listen() -> String {
    "0.0.0.0:9877".to_string()
}
fn d_rgb_fans() -> u8 {
    // RGB start-packet byte 3 = (channel<<4)+num_fans declares the LED chain
    // length; a value shorter than the physically-connected chain leaves the
    // tail fans dark (confirmed on hardware: 4 lit only 4/6 fans). 6 is the
    // hub's max chain length, so it's the only default that can't under-light
    // a full chain; shorter chains are unaffected (a uniform-fill command is
    // count-agnostic per rgb::LEDS_PER_CHANNEL's comment).
    6
}
fn d_rgb_brightness() -> u8 {
    0
}
fn d_rgb_buckets() -> u8 {
    8
}
fn d_true() -> bool {
    true
}
fn d_signal_alpha_one() -> f64 {
    1.0
}
fn d_margin_alpha() -> f64 {
    0.4
}
fn d_power_unit() -> PowerUnit {
    PowerUnit::Watts
}
fn d_power_rise_alpha() -> f64 {
    0.5
}
fn d_power_fall_alpha() -> f64 {
    0.1
}
fn d_signals_rise_alpha() -> f64 {
    0.5
}
fn d_signals_fall_alpha() -> f64 {
    0.1
}
fn d_throttle_floor_duty() -> u8 {
    85
}
fn d_throttle_hold_secs() -> u64 {
    30
}
fn d_throttle_reasons() -> Vec<ThrottleReason> {
    vec![
        ThrottleReason::SwThermal,
        ThrottleReason::HwThermal,
        ThrottleReason::HwPowerBrake,
    ]
}

fn d_rgb_stops() -> Vec<ColorStop> {
    vec![
        ColorStop {
            duty: 30,
            color: [0, 0, 255],
        },
        ColorStop {
            duty: 50,
            color: [0, 255, 0],
        },
        ColorStop {
            duty: 75,
            color: [255, 255, 0],
        },
        ColorStop {
            duty: 90,
            color: [255, 128, 0],
        },
        ColorStop {
            duty: 100,
            color: [255, 0, 0],
        },
    ]
}

/// Shared curve validator: non-empty, strictly increasing `temp` (the X
/// axis — die temp, watts, or headroom depending on the signal), and no
/// `duty` over 100. `name` prefixes every error message so callers (the
/// top-level `[[curve]]` and each `[signals.*]` curve) get a distinguishable
/// message from one implementation.
fn validate_curve(name: &str, points: &[CurvePoint]) -> Result<()> {
    if points.is_empty() {
        bail!("{name} must have at least one point");
    }
    for w in points.windows(2) {
        if w[1].temp <= w[0].temp {
            bail!("{name} temps must be strictly increasing");
        }
    }
    for p in points {
        if p.duty > 100 {
            bail!("{name} duty {} exceeds 100", p.duty);
        }
    }
    Ok(())
}

impl Config {
    /// Parses `s`, returning the config plus the dotted paths of any TOML
    /// keys that don't map to a known field (before validation runs).
    /// Unknown keys are intentionally NOT rejected (`deny_unknown_fields`
    /// would turn every downgrade/rollback with a newer config into a
    /// crashloop) — but a typo'd safety-relevant key like `fallbackduty`
    /// must not silently parse as if nothing were wrong, so callers log
    /// these. Split out from `from_str` so the collection can be tested
    /// without capturing log output.
    fn parse_with_ignored(s: &str) -> Result<(Config, Vec<String>)> {
        // toml v1: Deserializer::parse replaces ::new and returns a Result
        // (syntax errors surface here instead of during deserialization).
        let de = toml::Deserializer::parse(s).context("parsing config TOML")?;
        let mut ignored = Vec::new();
        let cfg: Config = serde_ignored::deserialize(de, |path| {
            ignored.push(path.to_string());
        })
        .context("parsing config TOML")?;
        Ok((cfg, ignored))
    }

    pub fn from_str(s: &str) -> Result<Config> {
        let (cfg, ignored) = Self::parse_with_ignored(s)?;
        for path in &ignored {
            log::warn!("ignoring unknown config key: {path}");
        }
        if cfg.signals.enabled && cfg.signals.thermal_margin.enabled {
            let duties: Vec<u8> = cfg
                .signals
                .thermal_margin
                .curve
                .iter()
                .map(|p| p.duty)
                .collect();
            if duties.windows(2).any(|w| w[1] > w[0]) {
                log::warn!(
                    "signals.margin.curve duty rises with headroom; more thermal headroom will command more fan — is the curve inverted?"
                );
            }
        }
        if cfg.signals.enabled
            && cfg.signals.throttle.enabled
            && cfg
                .signals
                .throttle
                .reasons
                .contains(&ThrottleReason::SwPowerCap)
        {
            log::warn!(
                "signals.throttle.reasons includes sw_power_cap, which MEASURED on this hardware asserts continuously (0x4) throughout a full inference load — including it will hold the throttle floor duty for the entire duration of any sustained load"
            );
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Config> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::from_str(&s)
    }

    fn validate(&self) -> Result<()> {
        validate_curve("curve", &self.curve)?;
        if self.fallback_duty > 100 {
            bail!("fallback_duty exceeds 100");
        }
        if self.channels.is_empty() {
            bail!("channels must not be empty");
        }
        for &c in &self.channels {
            if !(1..=4).contains(&c) {
                bail!("channel {} out of range 1..=4", c);
            }
        }
        if self.max_step_per_tick == 0 {
            bail!("max_step_per_tick must be nonzero (fans could never move)");
        }
        if self.poll_interval_secs == 0 {
            bail!("poll_interval_secs must be nonzero (busy-loop)");
        }
        if self.hysteresis_c < 0.0 {
            bail!("hysteresis_c must not be negative");
        }
        if self.cpu_offset < 0.0 {
            bail!("cpu_offset must not be negative");
        }
        if self.rgb.enabled {
            if self.rgb.stops.is_empty() {
                bail!("rgb.stops must have at least one stop when rgb is enabled");
            }
            for w in self.rgb.stops.windows(2) {
                if w[1].duty <= w[0].duty {
                    bail!("rgb.stops duties must be strictly increasing");
                }
            }
            for s in &self.rgb.stops {
                if s.duty > 100 {
                    bail!("rgb stop duty {} exceeds 100", s.duty);
                }
            }
            if !(1..=6).contains(&self.rgb.fans_per_channel) {
                bail!(
                    "rgb.fans_per_channel {} out of range 1..=6",
                    self.rgb.fans_per_channel
                );
            }
            for (k, &v) in &self.rgb.fans {
                let ch: u8 = k
                    .parse()
                    .map_err(|_| anyhow!("rgb.fans key {k:?} is not a valid channel number"))?;
                // Reject non-canonical keys like "01" or " 1": they parse
                // fine, but fans_for_channel() looks up channel.to_string()
                // ("1"), so a non-canonical key would validate successfully
                // and then silently never apply at runtime.
                if k != &ch.to_string() {
                    bail!("rgb.fans key {k:?} must be written as {:?}", ch.to_string());
                }
                if !self.channels.contains(&ch) {
                    bail!(
                        "rgb.fans key {} is not one of the configured channels {:?}",
                        ch,
                        self.channels
                    );
                }
                if !(1..=6).contains(&v) {
                    bail!("rgb.fans[{}] = {} out of range 1..=6", ch, v);
                }
            }
            if self.rgb.buckets < 2 {
                bail!("rgb.buckets must be at least 2");
            }
            if self.rgb.brightness > 8 {
                bail!(
                    "rgb.brightness {} out of range 0..=8 (0 = 100%)",
                    self.rgb.brightness
                );
            }
            if self.rgb.alerts.enabled {
                if self.rgb.alerts.sustained_after_secs < 1 {
                    bail!("rgb.alerts.sustained_after_secs must be at least 1");
                }
                if self.rgb.alerts.escalate_every_secs < 1 {
                    bail!("rgb.alerts.escalate_every_secs must be at least 1");
                }
                if self.rgb.alerts.cooldown_secs < 1 {
                    bail!("rgb.alerts.cooldown_secs must be at least 1");
                }
                if self.rgb.alerts.near_limit_margin_c < 0.0 {
                    bail!("rgb.alerts.near_limit_margin_c must not be negative");
                }
            }
        }
        if self.signals.enabled {
            let s = &self.signals;
            if !(s.gpu_temp.enabled
                || s.cpu_temp.enabled
                || s.gpu_power.enabled
                || s.thermal_margin.enabled
                || s.mem_temp.enabled)
            {
                bail!("signals.enabled but no sub-signal (gpu_temp/cpu_temp/power/margin/mem_temp) is enabled");
            }

            // Each sub-signal's checks are gated on that sub-signal's own
            // `enabled` flag — a disabled sub-signal's leftover/bad values
            // (stale curve, out-of-range alpha, ...) must not fail
            // validation, mirroring how `[rgb.alerts]` is only checked when
            // `rgb.alerts.enabled`.
            if s.gpu_temp.enabled {
                if !s.gpu_temp.curve.is_empty() {
                    validate_curve("signals.gpu_temp.curve", &s.gpu_temp.curve)?;
                }
                if !(s.gpu_temp.alpha > 0.0 && s.gpu_temp.alpha <= 1.0) {
                    bail!(
                        "signals.gpu_temp.alpha {} must be in (0.0, 1.0]",
                        s.gpu_temp.alpha
                    );
                }
            }

            if s.cpu_temp.enabled {
                if !s.cpu_temp.curve.is_empty() {
                    validate_curve("signals.cpu_temp.curve", &s.cpu_temp.curve)?;
                }
                if !(s.cpu_temp.alpha > 0.0 && s.cpu_temp.alpha <= 1.0) {
                    bail!(
                        "signals.cpu_temp.alpha {} must be in (0.0, 1.0]",
                        s.cpu_temp.alpha
                    );
                }
                if s.cpu_temp.offset_c < 0.0 {
                    bail!("signals.cpu_temp.offset_c must not be negative");
                }
            }

            // power/margin/mem_temp have no top-level curve fallback (their
            // X axes aren't die temperature), so an empty curve is always a
            // bail when the sub-signal is enabled, not a silent no-op.
            if s.gpu_power.enabled {
                validate_curve("signals.power.curve", &s.gpu_power.curve)?;
                if !(s.gpu_power.rise_alpha > 0.0 && s.gpu_power.rise_alpha <= 1.0) {
                    bail!(
                        "signals.power.rise_alpha {} must be in (0.0, 1.0]",
                        s.gpu_power.rise_alpha
                    );
                }
                if !(s.gpu_power.fall_alpha > 0.0 && s.gpu_power.fall_alpha <= 1.0) {
                    bail!(
                        "signals.power.fall_alpha {} must be in (0.0, 1.0]",
                        s.gpu_power.fall_alpha
                    );
                }
                if s.gpu_power.unit == PowerUnit::PercentTdp
                    && s.gpu_power.curve.iter().any(|p| p.temp > 200.0)
                {
                    bail!(
                        "signals.power.curve has a point > 200 with unit = \"percent_tdp\" — likely a watts/percent mixup"
                    );
                }
            }

            if s.thermal_margin.enabled {
                validate_curve("signals.margin.curve", &s.thermal_margin.curve)?;
                if !(s.thermal_margin.alpha > 0.0 && s.thermal_margin.alpha <= 1.0) {
                    bail!(
                        "signals.margin.alpha {} must be in (0.0, 1.0]",
                        s.thermal_margin.alpha
                    );
                }
            }

            if s.mem_temp.enabled {
                validate_curve("signals.mem_temp.curve", &s.mem_temp.curve)?;
                if !(s.mem_temp.alpha > 0.0 && s.mem_temp.alpha <= 1.0) {
                    bail!(
                        "signals.mem_temp.alpha {} must be in (0.0, 1.0]",
                        s.mem_temp.alpha
                    );
                }
            }

            if s.throttle.enabled {
                if s.throttle.floor_duty > 100 {
                    bail!(
                        "signals.throttle.floor_duty {} exceeds 100",
                        s.throttle.floor_duty
                    );
                }
                if s.throttle.reasons.is_empty() {
                    bail!("signals.throttle.reasons must not be empty when throttle is enabled");
                }
                if s.throttle.hold_secs > 3600 {
                    bail!(
                        "signals.throttle.hold_secs {} exceeds 3600",
                        s.throttle.hold_secs
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
poll_interval_secs = 5
channels = [1, 2]
fallback_duty = 60
cpu_offset = 10.0

[[curve]]
temp = 35.0
duty = 30
[[curve]]
temp = 55.0
duty = 40
[[curve]]
temp = 80.0
duty = 100

[metrics]
listen = "0.0.0.0:9877"
"#;

    #[test]
    fn parses_full_config() {
        let c = Config::from_str(FULL).unwrap();
        assert_eq!(c.poll_interval_secs, 5);
        assert_eq!(c.channels, vec![1, 2]);
        assert_eq!(c.fallback_duty, 60);
        assert_eq!(c.curve.len(), 3);
        assert_eq!(
            c.curve[2],
            CurvePoint {
                temp: 80.0,
                duty: 100
            }
        );
        assert_eq!(c.metrics.listen, "0.0.0.0:9877");
    }

    #[test]
    fn defaults_apply_when_omitted() {
        let c = Config::from_str("[[curve]]\ntemp = 40.0\nduty = 50\n").unwrap();
        assert_eq!(c.poll_interval_secs, 5);
        assert_eq!(c.channels, vec![1, 2]);
        assert_eq!(c.fallback_duty, 60);
        assert_eq!(c.cpu_offset, 10.0);
        assert_eq!(c.hysteresis_c, 2.0);
        assert_eq!(c.min_duty_delta, 5);
        assert_eq!(c.max_step_per_tick, 10);
        assert_eq!(c.metrics.listen, "0.0.0.0:9877");
    }

    #[test]
    fn rejects_empty_curve() {
        assert!(Config::from_str("channels = [1]\n").is_err());
    }

    #[test]
    fn rejects_unsorted_curve() {
        let s = "[[curve]]\ntemp = 60.0\nduty = 50\n[[curve]]\ntemp = 40.0\nduty = 30\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rejects_duty_over_100() {
        assert!(Config::from_str("[[curve]]\ntemp = 40.0\nduty = 101\n").is_err());
    }

    #[test]
    fn rejects_bad_channel() {
        let s = "channels = [1, 5]\n[[curve]]\ntemp = 40.0\nduty = 50\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rejects_zero_max_step_per_tick() {
        let s = "max_step_per_tick = 0\n[[curve]]\ntemp = 40.0\nduty = 50\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rejects_zero_poll_interval() {
        let s = "poll_interval_secs = 0\n[[curve]]\ntemp = 40.0\nduty = 50\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rejects_negative_hysteresis() {
        let s = "hysteresis_c = -1.0\n[[curve]]\ntemp = 40.0\nduty = 50\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rejects_negative_cpu_offset() {
        let s = "cpu_offset = -1.0\n[[curve]]\ntemp = 40.0\nduty = 50\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rgb_defaults_off_and_sane() {
        let c = Config::from_str("[[curve]]\ntemp = 40.0\nduty = 50\n").unwrap();
        assert!(!c.rgb.enabled);
        assert_eq!(c.rgb.fans_per_channel, 6);
        assert!(c.rgb.fans.is_empty());
        assert_eq!(c.rgb.brightness, 0);
        assert_eq!(c.rgb.buckets, 8);
        assert_eq!(c.rgb.stops.len(), 5);
        assert_eq!(
            c.rgb.stops[0],
            crate::rgb::ColorStop {
                duty: 30,
                color: [0, 0, 255]
            }
        );
    }

    #[test]
    fn rgb_section_parses() {
        let s = r#"
[[curve]]
temp = 40.0
duty = 50
[rgb]
enabled = true
fans_per_channel = 2
brightness = 1
buckets = 10
[[rgb.stops]]
duty = 0
color = [0, 0, 255]
[[rgb.stops]]
duty = 100
color = [255, 0, 0]
"#;
        let c = Config::from_str(s).unwrap();
        assert!(c.rgb.enabled);
        assert_eq!(c.rgb.fans_per_channel, 2);
        assert_eq!(c.rgb.stops.len(), 2);
    }

    #[test]
    fn rgb_validation_rejects_bad_values_when_enabled() {
        let base = "[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = true\n";
        // unsorted stops
        let s1 = format!("{base}[[rgb.stops]]\nduty = 50\ncolor = [1,1,1]\n[[rgb.stops]]\nduty = 30\ncolor = [2,2,2]\n");
        assert!(Config::from_str(&s1).is_err());
        // fans out of range
        assert!(Config::from_str(&format!("{base}fans_per_channel = 7\n")).is_err());
        // brightness out of range
        assert!(Config::from_str(&format!("{base}brightness = 9\n")).is_err());
        // buckets too small
        assert!(Config::from_str(&format!("{base}buckets = 1\n")).is_err());
    }

    #[test]
    fn rgb_bad_values_ignored_when_disabled() {
        // disabled section skips rgb validation
        let s = "[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = false\nfans_per_channel = 7\n";
        assert!(Config::from_str(s).is_ok());
    }

    #[test]
    fn alerts_defaults_off() {
        let c = Config::from_str("[[curve]]\ntemp = 40.0\nduty = 50\n").unwrap();
        assert!(!c.rgb.alerts.enabled);
        assert_eq!(c.rgb.alerts.sustained_hot_c, 75.0);
        assert_eq!(c.rgb.alerts.sustained_after_secs, 120);
        assert_eq!(c.rgb.alerts.near_limit_margin_c, 3.0);
        assert_eq!(c.rgb.alerts.escalate_every_secs, 30);
        assert_eq!(c.rgb.alerts.cooldown_secs, 60);
        assert_eq!(c.rgb.alerts.alert_color, [255, 0, 0]);
        assert_eq!(c.rgb.alerts.fault_colors, [[255, 80, 0], [255, 0, 0]]);
    }

    #[test]
    fn alerts_validation_rejects_zero_intervals() {
        let base = "[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = true\n[rgb.alerts]\nenabled = true\n";
        // escalate_every_secs = 0 should fail when enabled
        let s1 = format!("{base}escalate_every_secs = 0\n");
        assert!(Config::from_str(&s1).is_err());
        // but same value is ok when alerts disabled
        let s2 = "[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = true\n[rgb.alerts]\nenabled = false\nescalate_every_secs = 0\n";
        assert!(Config::from_str(s2).is_ok());
    }

    #[test]
    fn fans_for_channel_falls_back_to_fans_per_channel() {
        let s = "channels = [1, 2]\n[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = true\nfans_per_channel = 3\n";
        let c = Config::from_str(s).unwrap();
        assert_eq!(c.rgb.fans_for_channel(1), 3);
        assert_eq!(c.rgb.fans_for_channel(2), 3);
    }

    #[test]
    fn fans_for_channel_uses_per_channel_override() {
        let s = r#"
channels = [1, 2]
[[curve]]
temp = 40.0
duty = 50
[rgb]
enabled = true
fans_per_channel = 6
[rgb.fans]
1 = 3
2 = 6
"#;
        let c = Config::from_str(s).unwrap();
        assert_eq!(c.rgb.fans_for_channel(1), 3);
        assert_eq!(c.rgb.fans_for_channel(2), 6);
    }

    #[test]
    fn rgb_fans_map_defaults_empty() {
        let c = Config::from_str("[[curve]]\ntemp = 40.0\nduty = 50\n").unwrap();
        assert!(c.rgb.fans.is_empty());
    }

    #[test]
    fn rgb_fans_validation_rejects_channel_not_in_channels() {
        let s = "channels = [1, 2]\n[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = true\n[rgb.fans]\n3 = 4\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rgb_fans_validation_rejects_unparseable_key() {
        let s = "channels = [1]\n[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = true\n[rgb.fans]\nfoo = 4\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rgb_fans_validation_rejects_value_out_of_range() {
        let s = "channels = [1]\n[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = true\n[rgb.fans]\n1 = 7\n";
        assert!(Config::from_str(s).is_err());
        let s2 = "channels = [1]\n[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = true\n[rgb.fans]\n1 = 0\n";
        assert!(Config::from_str(s2).is_err());
    }

    #[test]
    fn rgb_fans_validation_rejects_non_canonical_key() {
        // "01" parses to channel 1 fine, but fans_for_channel() looks up
        // channel.to_string() == "1"; a non-canonical key would validate
        // and then silently never apply at runtime, so it must be rejected.
        let s = "channels = [1]\n[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = true\n[rgb.fans]\n\"01\" = 4\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rgb_fans_bad_values_ignored_when_rgb_disabled() {
        let s = "channels = [1]\n[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = false\n[rgb.fans]\n99 = 200\n";
        assert!(Config::from_str(s).is_ok());
    }

    #[test]
    fn unknown_top_level_key_is_collected_and_not_rejected() {
        let s = "fallbackduty = 90\n[[curve]]\ntemp = 40.0\nduty = 50\n";
        let (cfg, ignored) = Config::parse_with_ignored(s).unwrap();
        // typo silently falls back to the real default rather than erroring
        assert_eq!(cfg.fallback_duty, 60);
        assert!(ignored.iter().any(|p| p == "fallbackduty"), "{ignored:?}");
    }

    #[test]
    fn unknown_nested_key_is_collected_with_dotted_path() {
        let s = "[[curve]]\ntemp = 40.0\nduty = 50\n[rgb]\nenabled = true\ntypo_field = 1\n";
        let (_, ignored) = Config::parse_with_ignored(s).unwrap();
        assert!(ignored.iter().any(|p| p == "rgb.typo_field"), "{ignored:?}");
    }

    #[test]
    fn well_formed_config_has_no_ignored_keys() {
        let (_, ignored) = Config::parse_with_ignored(FULL).unwrap();
        assert!(ignored.is_empty(), "{ignored:?}");
    }

    // -- [signals] -----------------------------------------------------------

    #[test]
    fn signals_defaults_off_and_sane() {
        let c = Config::from_str("[[curve]]\ntemp = 40.0\nduty = 50\n").unwrap();
        assert!(!c.signals.enabled);
        assert_eq!(c.signals.rise_alpha, 0.5);
        assert_eq!(c.signals.fall_alpha, 0.1);

        assert!(c.signals.gpu_temp.enabled);
        assert!(c.signals.gpu_temp.curve.is_empty());
        assert_eq!(c.signals.gpu_temp.alpha, 1.0);

        assert!(c.signals.cpu_temp.enabled);
        assert_eq!(c.signals.cpu_temp.offset_c, 10.0);
        assert_eq!(c.signals.cpu_temp.alpha, 1.0);

        assert!(!c.signals.gpu_power.enabled);
        assert_eq!(c.signals.gpu_power.unit, PowerUnit::Watts);
        assert_eq!(c.signals.gpu_power.rise_alpha, 0.5);
        assert_eq!(c.signals.gpu_power.fall_alpha, 0.1);

        assert!(!c.signals.thermal_margin.enabled);
        assert_eq!(c.signals.thermal_margin.alpha, 0.4);

        assert!(!c.signals.mem_temp.enabled);
        assert_eq!(c.signals.mem_temp.alpha, 1.0);

        assert!(!c.signals.throttle.enabled);
        assert_eq!(c.signals.throttle.floor_duty, 85);
        assert_eq!(c.signals.throttle.hold_secs, 30);
        assert_eq!(
            c.signals.throttle.reasons,
            vec![
                ThrottleReason::SwThermal,
                ThrottleReason::HwThermal,
                ThrottleReason::HwPowerBrake,
            ]
        );
    }

    #[test]
    fn signals_section_parses() {
        let s = r#"
[[curve]]
temp = 40.0
duty = 50

[signals]
enabled = true

[signals.gpu_temp]
enabled = true

[signals.cpu_temp]
enabled = false
offset_c = 12.0

[signals.power]
enabled = true
unit = "percent_tdp"
[[signals.power.curve]]
temp = 25.0
duty = 30
[[signals.power.curve]]
temp = 95.0
duty = 100

[signals.margin]
enabled = true
[[signals.margin.curve]]
temp = 5.0
duty = 100
[[signals.margin.curve]]
temp = 35.0
duty = 30

[signals.mem_temp]
enabled = true
[[signals.mem_temp.curve]]
temp = 60.0
duty = 40
[[signals.mem_temp.curve]]
temp = 90.0
duty = 100

[signals.throttle]
enabled = true
floor_duty = 90
hold_secs = 45
reasons = ["sw_thermal", "hw_power_brake"]
"#;
        let c = Config::from_str(s).unwrap();
        assert!(c.signals.enabled);
        assert!(c.signals.gpu_temp.enabled);
        assert!(!c.signals.cpu_temp.enabled);
        assert_eq!(c.signals.cpu_temp.offset_c, 12.0);
        assert!(c.signals.gpu_power.enabled);
        assert_eq!(c.signals.gpu_power.unit, PowerUnit::PercentTdp);
        assert_eq!(c.signals.gpu_power.curve.len(), 2);
        assert!(c.signals.thermal_margin.enabled);
        assert_eq!(c.signals.thermal_margin.curve.len(), 2);
        assert!(c.signals.mem_temp.enabled);
        assert!(c.signals.throttle.enabled);
        assert_eq!(c.signals.throttle.floor_duty, 90);
        assert_eq!(c.signals.throttle.hold_secs, 45);
        assert_eq!(
            c.signals.throttle.reasons,
            vec![ThrottleReason::SwThermal, ThrottleReason::HwPowerBrake]
        );
    }

    #[test]
    fn curve_point_accepts_value_alias() {
        let s = r#"
[[curve]]
temp = 40.0
duty = 50
[signals]
enabled = true
[signals.power]
enabled = true
unit = "watts"
[[signals.power.curve]]
value = 120.0
duty = 30
[[signals.power.curve]]
value = 560.0
duty = 100
"#;
        let c = Config::from_str(s).unwrap();
        assert_eq!(c.signals.gpu_power.curve[0].temp, 120.0);
        assert_eq!(c.signals.gpu_power.curve[1].temp, 560.0);
    }

    #[test]
    fn rejects_signals_enabled_with_no_signal_enabled() {
        let s = "[[curve]]\ntemp = 40.0\nduty = 50\n[signals]\nenabled = true\n[signals.gpu_temp]\nenabled = false\n[signals.cpu_temp]\nenabled = false\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rejects_power_enabled_without_curve() {
        let s = "[[curve]]\ntemp = 40.0\nduty = 50\n[signals]\nenabled = true\n[signals.power]\nenabled = true\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rejects_margin_enabled_without_curve() {
        let s = "[[curve]]\ntemp = 40.0\nduty = 50\n[signals]\nenabled = true\n[signals.margin]\nenabled = true\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rejects_alpha_out_of_range() {
        let base = "[[curve]]\ntemp = 40.0\nduty = 50\n[signals]\nenabled = true\n[signals.gpu_temp]\nenabled = true\n";
        for bad in ["0.0", "-0.1", "1.5"] {
            let s = format!("{base}alpha = {bad}\n");
            assert!(
                Config::from_str(&s).is_err(),
                "alpha={bad} should be rejected"
            );
        }
        let ok = format!("{base}alpha = 1.0\n");
        assert!(Config::from_str(&ok).is_ok());
    }

    #[test]
    fn rejects_percent_tdp_curve_over_200() {
        let s = r#"
[[curve]]
temp = 40.0
duty = 50
[signals]
enabled = true
[signals.power]
enabled = true
unit = "percent_tdp"
[[signals.power.curve]]
temp = 120.0
duty = 30
[[signals.power.curve]]
temp = 560.0
duty = 100
"#;
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rejects_throttle_floor_over_100() {
        let s = "[[curve]]\ntemp = 40.0\nduty = 50\n[signals]\nenabled = true\n[signals.throttle]\nenabled = true\nfloor_duty = 101\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn rejects_negative_signals_cpu_offset() {
        let s = "[[curve]]\ntemp = 40.0\nduty = 50\n[signals]\nenabled = true\n[signals.cpu_temp]\nenabled = true\noffset_c = -1.0\n";
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn signals_bad_values_ignored_when_disabled() {
        // disabled section skips signals validation entirely
        let s = "[[curve]]\ntemp = 40.0\nduty = 50\n[signals]\nenabled = false\n[signals.throttle]\nenabled = true\nfloor_duty = 200\n";
        assert!(Config::from_str(s).is_ok());
    }

    #[test]
    fn signals_sub_signal_bad_values_ignored_when_sub_signal_disabled() {
        // gpu_temp stays enabled (its default), so signals.enabled has a
        // live sub-signal; cpu_temp's out-of-range values must be ignored
        // because cpu_temp itself is disabled.
        let s = "[[curve]]\ntemp = 40.0\nduty = 50\n[signals]\nenabled = true\n[signals.cpu_temp]\nenabled = false\noffset_c = -5.0\nalpha = 5.0\n";
        assert!(Config::from_str(s).is_ok());
    }

    #[test]
    fn unknown_signals_key_is_collected_with_dotted_path() {
        let s = "[[curve]]\ntemp = 40.0\nduty = 50\n[signals]\nenabled = true\ntypo_field = 1\n";
        let (_, ignored) = Config::parse_with_ignored(s).unwrap();
        assert!(
            ignored.iter().any(|p| p == "signals.typo_field"),
            "{ignored:?}"
        );
    }
}
