use anyhow::{Context, Result};
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{TemperatureSensor, TemperatureThreshold};
use nvml_wrapper::enums::device::SampleValue;
use nvml_wrapper::error::NvmlError;
use nvml_wrapper::structs::device::FieldId;
use nvml_wrapper::sys_exports::field_id;
use nvml_wrapper::{Device, Nvml};
use std::path::{Path, PathBuf};

use crate::signals::{GpuReading, ThrottleFlags};

/// Pick the control temperature: hotter of GPU and offset CPU.
/// Degraded (second element) when the GPU signal is unavailable.
pub fn control_temp(gpu: Option<f64>, cpu: Option<f64>, cpu_offset: f64) -> (Option<f64>, bool) {
    match (gpu, cpu) {
        (Some(g), Some(c)) => (Some(g.max(c - cpu_offset)), false),
        (Some(g), None) => (Some(g), false),
        (None, Some(c)) => (Some(c), true),
        (None, None) => (None, true),
    }
}

/// Minimum ticks between periodic re-discovery attempts for a missing or
/// persistently-failing sensor (~60s at the default 5s poll interval).
pub const REDISCOVERY_INTERVAL_TICKS: u32 = 12;

/// Pure cadence check for periodic sensor re-discovery: retry at most once
/// every `interval` ticks, and only while the sensor is actually
/// absent/failing. `ticks_since_probe` counts ticks since the last attempt
/// (whether or not it succeeded).
pub fn should_reprobe(missing_or_failing: bool, ticks_since_probe: u32, interval: u32) -> bool {
    missing_or_failing && ticks_since_probe >= interval
}

/// Where `GpuSensor::probe_caps` sourced (or failed to source) the thermal
/// margin signal.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum MarginSource {
    #[default]
    Unavailable,
    /// Dead on nvml-wrapper 0.12.1, BY DESIGN, and expected to STAY dead
    /// even now that the rest of this module is wired in: NVML fields
    /// 193/194/196 report driver value type 5, which this crate version's
    /// `SampleValue` enum (types 0-4 only) cannot decode, so every read
    /// comes back `Err(UnexpectedVariant(5))` (see
    /// `docs/superpowers/plans/wave-0-findings.md`, DEFINITIVE section).
    /// Kept for a future crate version that can decode type 5;
    /// `probe_caps` must never select it. Do not read this as an
    /// oversight if it's still flagged dead.
    #[allow(dead_code)]
    TlimitField,
    GpuMaxMinusTemp {
        threshold_c: f64,
    },
}

/// One GPU's multi-signal capabilities, detected by `GpuSensor::probe_all`
/// at init and on re-discovery, rather than re-probed every tick.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GpuCaps {
    pub power: bool,
    pub margin: MarginSource,
    pub mem_temp: bool,
    pub throttle: bool,
    pub power_limit_w: Option<f64>,
}

/// True only for a REAL read failure (anything other than `NotSupported`)
/// on the corresponding signal, for the current tick. `NotSupported` is
/// normal absence, not an error, and never sets these.
#[derive(Debug, Clone, Copy, Default)]
pub struct SignalErrors {
    pub power: bool,
    pub margin: bool,
    pub mem_temp: bool,
    pub throttle: bool,
}

/// Minimum ticks between periodic refreshes of the cached enforced power
/// limit (~5 min at the default 5 s poll interval). `nvidia-smi -pl` can
/// change the limit at runtime; reuses the existing `should_reprobe` cadence
/// helper rather than adding a parallel mechanism.
pub const POWER_LIMIT_REFRESH_TICKS: u32 = 60;

/// Shared error policy for every NVML signal read: `NotSupported` is normal
/// absence for silicon that lacks the signal (never logged per-tick, never
/// counted as an error); any other error is a real failure (`log::debug!`
/// plus the error flag).
fn classify_read<T>(result: std::result::Result<T, NvmlError>, error_flag: &mut bool) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(NvmlError::NotSupported) => None,
        Err(e) => {
            log::debug!("nvml signal read failed: {e}");
            *error_flag = true;
            None
        }
    }
}

/// Pure arithmetic for `MarginSource::GpuMaxMinusTemp`: headroom to the
/// static GpuMax threshold. Extracted so it's testable without a GPU.
/// `None` temp (absent or a real read failure) propagates to `None`.
///
/// Exercised directly by `margin_from_threshold_computes_headroom` as well
/// as by `read_all`.
pub fn margin_from_threshold(threshold_c: f64, temp_c: Option<f64>) -> Option<f64> {
    temp_c.map(|t| threshold_c - t)
}

/// Maps the four throttle-reason bits this daemon tracks for fan control
/// into `ThrottleFlags`. Reasons not tracked here (e.g. `GPU_IDLE`,
/// `SYNC_BOOST`) must never leak into a tracked flag.
pub fn throttle_flags(r: ThrottleReasons) -> ThrottleFlags {
    ThrottleFlags {
        sw_thermal: r.contains(ThrottleReasons::SW_THERMAL_SLOWDOWN),
        hw_thermal: r.contains(ThrottleReasons::HW_THERMAL_SLOWDOWN),
        hw_power_brake: r.contains(ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN),
        sw_power_cap: r.contains(ThrottleReasons::SW_POWER_CAP),
    }
}

/// Extracts the numeric payload from any `SampleValue` variant, handling all
/// four (including a negative `I64`).
pub fn sample_to_f64(v: SampleValue) -> f64 {
    match v {
        SampleValue::F64(f) => f,
        SampleValue::U32(u) => u as f64,
        SampleValue::U64(u) => u as f64,
        SampleValue::I64(i) => i as f64,
    }
}

pub struct CpuSensor {
    temp_path: PathBuf,
}

impl CpuSensor {
    pub fn discover_in(base: &Path) -> Option<CpuSensor> {
        for entry in std::fs::read_dir(base).ok()? {
            let dir = entry.ok()?.path();
            let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
            if name.trim() == "k10temp" {
                return Some(CpuSensor {
                    temp_path: dir.join("temp1_input"),
                });
            }
        }
        None
    }

    pub fn discover() -> Option<CpuSensor> {
        Self::discover_in(Path::new("/sys/class/hwmon"))
    }

    pub fn read_c(&self) -> Result<f64> {
        let raw = std::fs::read_to_string(&self.temp_path)
            .with_context(|| format!("reading {}", self.temp_path.display()))?;
        let milli: f64 = raw.trim().parse().context("parsing k10temp value")?;
        Ok(milli / 1000.0)
    }
}

/// Shared field-id slice for the batched `field_values_for` probe of NVML
/// field 82 (MEMORY_TEMP), used identically by `probe_caps` (capability
/// detection) and `read_all` (the per-tick read) so the two call sites
/// can't drift as more field-based signals are added.
const MEM_TEMP_FIELD_IDS: [FieldId; 1] = [FieldId(field_id::NVML_FI_DEV_MEMORY_TEMP)];

/// One NVML device as found by `GpuSensor::probe_all`: its identity plus
/// the capabilities probed for it. Cards are probed individually because
/// they need not match — a workstation card may expose signals (memory
/// temperature, say) that a consumer card beside it does not.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub uuid: String,
    pub caps: GpuCaps,
}

/// Pure check for GPU re-discovery: true when a re-probe found nothing new,
/// so the caller can keep its filter state. Requires that some card was
/// still answering on the old handle (`any_card_answering`) — when none
/// was, the handle itself is suspect (driver restart) and an identical
/// card list still warrants a full reset.
pub fn cards_unchanged(any_card_answering: bool, old: &[GpuInfo], new: &[GpuInfo]) -> bool {
    any_card_answering && old == new
}

pub struct GpuSensor {
    nvml: Nvml,
}

impl GpuSensor {
    /// None when libnvidia-ml.so is absent (no nvidia runtime class) or no
    /// GPU can be opened.
    pub fn init() -> Option<GpuSensor> {
        let nvml = Nvml::init().ok()?;
        let count = nvml.device_count().ok()?;
        (0..count)
            .any(|i| nvml.device_by_index(i).is_ok())
            .then_some(GpuSensor { nvml })
    }

    pub fn device_count(&self) -> u32 {
        self.nvml.device_count().unwrap_or(0)
    }

    /// One NVML probing round over every device, run at init and on
    /// re-discovery rather than per tick. A device that can't be opened is
    /// still listed (with no capabilities) so its failing reads stay
    /// visible and keep driving re-discovery, instead of the card silently
    /// dropping out of fan control.
    pub fn probe_all(&self) -> Vec<GpuInfo> {
        (0..self.device_count())
            .map(|index| match self.nvml.device_by_index(index) {
                Ok(dev) => GpuInfo {
                    index,
                    name: dev.name().unwrap_or_default(),
                    uuid: dev.uuid().unwrap_or_default(),
                    caps: Self::probe_caps(&dev),
                },
                Err(e) => {
                    log::warn!("gpu {index} probe: nvml device: {e}");
                    GpuInfo {
                        index,
                        name: String::new(),
                        uuid: String::new(),
                        caps: GpuCaps::default(),
                    }
                }
            })
            .collect()
    }

    /// Detects each multi-signal capability of one device by attempting
    /// the read once.
    ///
    /// Margin: `MarginSource::TlimitField` (fields 193/194/196) is DEAD on
    /// this crate version — nvml-wrapper 0.12.1's `SampleValue` enum only
    /// decodes NVML value types 0-4, but the driver reports these fields as
    /// type 5, so every read comes back `Err(UnexpectedVariant(5))`. This is
    /// a crate limitation, not a hardware one (nvidia-smi reads the same
    /// fields fine); the variant stays defined for a future crate version,
    /// but `probe_caps` must never select it. Instead we use
    /// `temperature_threshold(GpuMax) - temperature(Gpu)`, which the Wave 0
    /// spike confirmed returns a clean, stable value (`Ok(90)` on that card).
    fn probe_caps(dev: &Device) -> GpuCaps {
        let power = dev.power_usage().is_ok();

        let margin = match dev.temperature_threshold(TemperatureThreshold::GpuMax) {
            Ok(t) => MarginSource::GpuMaxMinusTemp {
                threshold_c: t as f64,
            },
            Err(_) => MarginSource::Unavailable,
        };

        // field 82 MEMORY_TEMP: probed via the batched field API, matching
        // how read_all will read it, since a device-attribute-style
        // probe isn't available for this one.
        let mem_temp = {
            match dev.field_values_for(&MEM_TEMP_FIELD_IDS) {
                Ok(samples) => samples
                    .into_iter()
                    .next()
                    .map(|s| matches!(s, Ok(fv) if fv.value.is_ok()))
                    .unwrap_or(false),
                Err(_) => false,
            }
        };

        let throttle = dev.current_throttle_reasons().is_ok();

        let power_limit_w = dev.enforced_power_limit().ok().map(|mw| mw as f64 / 1000.0);

        GpuCaps {
            power,
            margin,
            mem_temp,
            throttle,
            power_limit_w,
        }
    }

    /// Reads every card for one tick, attempting only the capabilities
    /// found present on each. Never returns `Err` and never panics. Die
    /// temperature is the one read every card must answer: its failure is
    /// logged at warn and leaves `temp_c` `None`, which the caller treats
    /// as a failed tick for re-discovery. Every other NVML failure is
    /// folded into `SignalErrors` (real failures, OR-ed across cards) or
    /// silent absence (`NotSupported`) via `classify_read`.
    pub fn read_all(&self, gpus: &[GpuInfo]) -> (Vec<GpuReading>, SignalErrors) {
        let mut errors = SignalErrors::default();
        let readings = gpus
            .iter()
            .map(|info| {
                let (reading, e) = self.read_one(info);
                errors.power |= e.power;
                errors.margin |= e.margin;
                errors.mem_temp |= e.mem_temp;
                errors.throttle |= e.throttle;
                reading
            })
            .collect();
        (readings, errors)
    }

    fn read_one(&self, info: &GpuInfo) -> (GpuReading, SignalErrors) {
        let caps = &info.caps;
        let mut errors = SignalErrors::default();
        let mut reading = GpuReading {
            index: info.index,
            name: info.name.clone(),
            uuid: info.uuid.clone(),
            power_limit_w: caps.power_limit_w,
            ..GpuReading::default()
        };

        let dev = match self.nvml.device_by_index(info.index) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("gpu {} read: nvml device: {e}", info.index);
                reading.read_error = true;
                return (reading, errors);
            }
        };

        match dev.temperature(TemperatureSensor::Gpu) {
            Ok(t) => reading.temp_c = Some(t as f64),
            Err(e) => {
                log::warn!("gpu {} read: nvml temperature: {e}", info.index);
                reading.read_error = true;
            }
        }

        if caps.power {
            reading.power_w = classify_read(
                dev.power_usage().map(|mw| mw as f64 / 1000.0),
                &mut errors.power,
            );
        }

        if let MarginSource::GpuMaxMinusTemp { threshold_c } = caps.margin {
            reading.thermal_margin_c = margin_from_threshold(threshold_c, reading.temp_c);
            errors.margin = reading.temp_c.is_none();
        }

        if caps.mem_temp {
            let result: std::result::Result<f64, NvmlError> =
                match dev.field_values_for(&MEM_TEMP_FIELD_IDS) {
                    Ok(samples) => match samples.into_iter().next() {
                        Some(Ok(fv)) => fv.value.map(sample_to_f64),
                        Some(Err(e)) => Err(e),
                        None => Err(NvmlError::Unknown),
                    },
                    Err(e) => Err(e),
                };
            reading.mem_temp_c = classify_read(result, &mut errors.mem_temp);
        }

        if caps.throttle {
            reading.throttle = classify_read(dev.current_throttle_reasons(), &mut errors.throttle)
                .map(throttle_flags);
        }

        reading.read_error |= errors.power || errors.margin || errors.mem_temp || errors.throttle;
        (reading, errors)
    }

    /// Re-reads each card's `enforced_power_limit` into its cached caps.
    /// `nvidia-smi -pl` can change the enforced limit at runtime, and a
    /// stale cached value would silently rescale a `percent_tdp` curve.
    /// Called on the `POWER_LIMIT_REFRESH_TICKS` cadence via the existing
    /// `should_reprobe` helper, not a parallel mechanism.
    pub fn refresh_power_limits(&self, gpus: &mut [GpuInfo]) {
        for info in gpus {
            let Ok(dev) = self.nvml.device_by_index(info.index) else {
                continue;
            };
            info.caps.power_limit_w = dev.enforced_power_limit().ok().map(|mw| mw as f64 / 1000.0);
        }
    }

    /// Read-only capability/diagnostic dump for `--probe-gpu`. Never panics
    /// and never returns `Err`; every individual NVML failure becomes an
    /// `Err(String)` in place so the struct is pure data.
    pub fn probe_snapshot(&self, index: u32) -> ProbeSnapshot {
        let nvml_version = self.nvml.sys_nvml_version().map_err(|e| e.to_string());
        let driver_version = self.nvml.sys_driver_version().map_err(|e| e.to_string());

        let dev = match self.nvml.device_by_index(index) {
            Ok(d) => d,
            Err(e) => {
                let msg = e.to_string();
                return ProbeSnapshot {
                    nvml_version,
                    driver_version,
                    device_name: Err(msg.clone()),
                    power_usage_mw: Err(msg.clone()),
                    enforced_power_limit_mw: Err(msg.clone()),
                    power_management_limit_mw: Err(msg.clone()),
                    power_limit_constraints_mw: Err(msg.clone()),
                    temperature_gpu_c: Err(msg.clone()),
                    thresholds: THRESHOLD_KINDS
                        .iter()
                        .map(|&(name, _)| (name, Err(msg.clone())))
                        .collect(),
                    fields: FIELD_IDS
                        .iter()
                        .map(|&(id, name)| (id, name, Err(msg.clone())))
                        .collect(),
                    throttle_raw: Err(msg.clone()),
                    throttle_reasons: Err(msg),
                };
            }
        };

        let device_name = dev.name().map_err(|e| e.to_string());
        let power_usage_mw = dev.power_usage().map_err(|e| e.to_string());
        let enforced_power_limit_mw = dev.enforced_power_limit().map_err(|e| e.to_string());
        let power_management_limit_mw = dev.power_management_limit().map_err(|e| e.to_string());
        let power_limit_constraints_mw = dev
            .power_management_limit_constraints()
            .map(|c| (c.min_limit, c.max_limit))
            .map_err(|e| e.to_string());
        let temperature_gpu_c = dev
            .temperature(TemperatureSensor::Gpu)
            .map_err(|e| e.to_string());

        let thresholds = THRESHOLD_KINDS
            .iter()
            .map(|&(name, kind)| {
                (
                    name,
                    dev.temperature_threshold(kind).map_err(|e| e.to_string()),
                )
            })
            .collect();

        let ids: Vec<FieldId> = FIELD_IDS.iter().map(|&(id, _)| FieldId(id)).collect();
        let batched = if ids.is_empty() {
            Ok(Vec::new())
        } else {
            dev.field_values_for(&ids)
        };
        let fields = match batched {
            // Pad rather than zip-truncate: nvml-wrapper 0.12.1 returns exactly
            // one sample per requested id, but if that invariant ever changed a
            // bare zip would silently drop field lines -- and "prints every
            // field" is this tool's whole contract. A short result now shows up
            // as an explicit per-field error instead of a vanished line.
            Ok(mut samples) => {
                if samples.len() < FIELD_IDS.len() {
                    let missing = FIELD_IDS.len() - samples.len();
                    samples
                        .extend(std::iter::repeat_with(|| Err(NvmlError::Unknown)).take(missing));
                }
                FIELD_IDS
                    .iter()
                    .zip(samples)
                    .map(|(&(id, name), sample)| {
                        let rendered = match sample {
                            Ok(fv) => match fv.value {
                                Ok(v) => Ok(format!("{v:?}")),
                                Err(e) => Err(e.to_string()),
                            },
                            Err(e) => Err(e.to_string()),
                        };
                        (id, name, rendered)
                    })
                    .collect()
            }
            Err(e) => {
                let msg = e.to_string();
                FIELD_IDS
                    .iter()
                    .map(|&(id, name)| (id, name, Err(msg.clone())))
                    .collect()
            }
        };

        let (throttle_raw, throttle_reasons) = match dev.current_throttle_reasons() {
            Ok(r) => (
                Ok(r.bits()),
                Ok(r.iter_names().map(|(name, _)| name).collect()),
            ),
            Err(e) => {
                let msg = e.to_string();
                (Err(msg.clone()), Err(msg))
            }
        };

        ProbeSnapshot {
            nvml_version,
            driver_version,
            device_name,
            power_usage_mw,
            enforced_power_limit_mw,
            power_management_limit_mw,
            power_limit_constraints_mw,
            temperature_gpu_c,
            thresholds,
            fields,
            throttle_raw,
            throttle_reasons,
        }
    }
}

/// All eight `TemperatureThreshold` variants probed by `probe_snapshot`, with
/// the label used in `ProbeSnapshot::thresholds`/`format_probe` output.
const THRESHOLD_KINDS: [(&str, TemperatureThreshold); 8] = [
    ("Shutdown", TemperatureThreshold::Shutdown),
    ("Slowdown", TemperatureThreshold::Slowdown),
    ("MemoryMax", TemperatureThreshold::MemoryMax),
    ("GpuMax", TemperatureThreshold::GpuMax),
    ("AcousticMin", TemperatureThreshold::AcousticMin),
    ("AcousticCurr", TemperatureThreshold::AcousticCurr),
    ("AcousticMax", TemperatureThreshold::AcousticMax),
    ("GpsCurr", TemperatureThreshold::GpsCurr),
];

/// Field IDs batched by `probe_snapshot` via a single `field_values_for`
/// call, in the order the Wave 0 spike cares about them.
const FIELD_IDS: [(u32, &str); 7] = [
    (field_id::NVML_FI_DEV_POWER_AVERAGE, "POWER_AVERAGE"),
    (field_id::NVML_FI_DEV_POWER_INSTANT, "POWER_INSTANT"),
    (field_id::NVML_FI_DEV_MEMORY_TEMP, "MEMORY_TEMP"),
    (
        field_id::NVML_FI_DEV_TEMPERATURE_SHUTDOWN_TLIMIT,
        "SHUTDOWN_TLIMIT",
    ),
    (
        field_id::NVML_FI_DEV_TEMPERATURE_SLOWDOWN_TLIMIT,
        "SLOWDOWN_TLIMIT",
    ),
    (
        field_id::NVML_FI_DEV_TEMPERATURE_MEM_MAX_TLIMIT,
        "MEM_MAX_TLIMIT",
    ),
    (
        field_id::NVML_FI_DEV_TEMPERATURE_GPU_MAX_TLIMIT,
        "GPU_MAX_TLIMIT",
    ),
];

/// Plain-data result of `GpuSensor::probe_snapshot`: everything the
/// `--probe-gpu` diagnostic dump collects, with each fallible item already
/// stringified so `format_probe` needs no `nvml_wrapper` types.
pub struct ProbeSnapshot {
    pub nvml_version: Result<String, String>,
    pub driver_version: Result<String, String>,
    pub device_name: Result<String, String>,
    pub power_usage_mw: Result<u32, String>,
    pub enforced_power_limit_mw: Result<u32, String>,
    pub power_management_limit_mw: Result<u32, String>,
    pub power_limit_constraints_mw: Result<(u32, u32), String>,
    pub temperature_gpu_c: Result<u32, String>,
    pub thresholds: Vec<(&'static str, Result<u32, String>)>,
    pub fields: Vec<(u32, &'static str, Result<String, String>)>,
    pub throttle_raw: Result<u64, String>,
    pub throttle_reasons: Result<Vec<&'static str>, String>,
}

/// Extracts the numeric payload out of a rendered `SampleValue` debug string
/// (e.g. `"I64(-3)"` -> `-3.0`), used for the derived-margin lines below.
fn extract_numeric(rendered: &str) -> Option<f64> {
    let open = rendered.find('(')?;
    let close = rendered.rfind(')')?;
    rendered.get(open + 1..close)?.trim().parse::<f64>().ok()
}

/// Pure formatter (no NVML) for a `ProbeSnapshot`: one `key=value` line per
/// item, printing every field even when absent so nothing is silently
/// suppressed. Also prints two derived margin lines when their inputs are
/// available.
pub fn format_probe(s: &ProbeSnapshot) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "nvml_version={:?}", s.nvml_version);
    let _ = writeln!(out, "driver_version={:?}", s.driver_version);
    let _ = writeln!(out, "device_name={:?}", s.device_name);
    let _ = writeln!(out, "power_usage_mw={:?}", s.power_usage_mw);
    let _ = writeln!(
        out,
        "enforced_power_limit_mw={:?}",
        s.enforced_power_limit_mw
    );
    let _ = writeln!(
        out,
        "power_management_limit_mw={:?}",
        s.power_management_limit_mw
    );
    let _ = writeln!(
        out,
        "power_limit_constraints_mw={:?}",
        s.power_limit_constraints_mw
    );
    let _ = writeln!(out, "temperature_gpu_c={:?}", s.temperature_gpu_c);

    for (name, r) in &s.thresholds {
        let _ = writeln!(out, "threshold.{name}={r:?}");
    }
    for (id, name, r) in &s.fields {
        let _ = writeln!(out, "field.{id}.{name}={r:?}");
    }

    let _ = writeln!(out, "throttle_raw={:?}", s.throttle_raw);
    let _ = writeln!(out, "throttle_reasons={:?}", s.throttle_reasons);

    let field196 = s
        .fields
        .iter()
        .find(|(_, name, _)| *name == "GPU_MAX_TLIMIT")
        .and_then(|(_, _, r)| r.as_ref().ok());
    if let Some(v) = field196.and_then(|rendered| extract_numeric(rendered)) {
        let _ = writeln!(out, "derived.margin_via_field196_c={v}");
    }

    let gpu_max_threshold = s
        .thresholds
        .iter()
        .find(|(name, _)| *name == "GpuMax")
        .and_then(|(_, r)| r.as_ref().ok())
        .copied();
    let current_temp = s.temperature_gpu_c.as_ref().ok().copied();
    if let (Some(limit), Some(temp)) = (gpu_max_threshold, current_temp) {
        let margin = limit as f64 - temp as f64;
        let _ = writeln!(out, "derived.margin_via_threshold_c={margin}");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_temp_takes_hotter_of_gpu_and_offset_cpu() {
        assert_eq!(
            control_temp(Some(70.0), Some(60.0), 10.0),
            (Some(70.0), false)
        );
        assert_eq!(
            control_temp(Some(40.0), Some(85.0), 10.0),
            (Some(75.0), false)
        );
    }

    #[test]
    fn control_temp_degrades_gracefully() {
        // GPU missing: raw CPU temp, degraded
        assert_eq!(control_temp(None, Some(60.0), 10.0), (Some(60.0), true));
        // CPU missing: GPU only, not degraded
        assert_eq!(control_temp(Some(70.0), None, 10.0), (Some(70.0), false));
        // both missing
        assert_eq!(control_temp(None, None, 10.0), (None, true));
    }

    #[test]
    fn cpu_sensor_discovers_k10temp_and_reads_millidegrees() {
        let base = tempfile::tempdir().unwrap();
        let h0 = base.path().join("hwmon0");
        std::fs::create_dir_all(&h0).unwrap();
        std::fs::write(h0.join("name"), "nvme\n").unwrap();
        let h1 = base.path().join("hwmon1");
        std::fs::create_dir_all(&h1).unwrap();
        std::fs::write(h1.join("name"), "k10temp\n").unwrap();
        std::fs::write(h1.join("temp1_input"), "64250\n").unwrap();

        let s = CpuSensor::discover_in(base.path()).unwrap();
        assert!((s.read_c().unwrap() - 64.25).abs() < 1e-9);
    }

    #[test]
    fn cpu_sensor_none_when_absent() {
        let base = tempfile::tempdir().unwrap();
        assert!(CpuSensor::discover_in(base.path()).is_none());
    }

    #[test]
    fn should_reprobe_only_when_missing_and_interval_elapsed() {
        assert!(!should_reprobe(false, 100, 12)); // healthy: never reprobe
        assert!(!should_reprobe(true, 11, 12)); // missing, but too soon
        assert!(should_reprobe(true, 12, 12)); // missing, interval reached
        assert!(should_reprobe(true, 20, 12)); // missing, well past interval
    }

    #[test]
    fn should_reprobe_at_tick_zero_when_never_probed() {
        // A sensor missing from the first tick (ticks_since_probe starts at
        // 0) must wait a full interval before the first reprobe, not retry
        // every tick.
        assert!(!should_reprobe(true, 0, 12));
    }

    #[test]
    fn cards_unchanged_only_when_handle_alive_and_probe_identical() {
        let card = |index: u32, power: bool| GpuInfo {
            index,
            name: "RTX".to_string(),
            uuid: format!("GPU-{index}"),
            caps: GpuCaps {
                power,
                ..GpuCaps::default()
            },
        };
        let old = [card(0, true), card(1, true)];

        // One card dead, the other answering, same probe: keep state.
        assert!(cards_unchanged(true, &old, &old.clone()));
        // No card answering: the handle is suspect, reset even if identical.
        assert!(!cards_unchanged(false, &old, &old.clone()));
        // A capability changed, or a card appeared/vanished: reset.
        assert!(!cards_unchanged(
            true,
            &old,
            &[card(0, true), card(1, false)]
        ));
        assert!(!cards_unchanged(true, &old, &[card(0, true)]));
    }

    fn absent<T>() -> Result<T, String> {
        Err("NotSupported".to_string())
    }

    fn absent_snapshot() -> ProbeSnapshot {
        ProbeSnapshot {
            nvml_version: absent(),
            driver_version: absent(),
            device_name: absent(),
            power_usage_mw: absent(),
            enforced_power_limit_mw: absent(),
            power_management_limit_mw: absent(),
            power_limit_constraints_mw: absent(),
            temperature_gpu_c: absent(),
            thresholds: THRESHOLD_KINDS
                .iter()
                .map(|&(name, _)| (name, absent()))
                .collect(),
            fields: FIELD_IDS
                .iter()
                .map(|&(id, name)| (id, name, absent()))
                .collect(),
            throttle_raw: absent(),
            throttle_reasons: absent(),
        }
    }

    #[test]
    fn format_probe_renders_all_absent_signals() {
        let snap = absent_snapshot();
        let out = format_probe(&snap);

        assert!(!out.is_empty());
        for key in [
            "nvml_version=",
            "driver_version=",
            "device_name=",
            "power_usage_mw=",
            "enforced_power_limit_mw=",
            "power_management_limit_mw=",
            "power_limit_constraints_mw=",
            "temperature_gpu_c=",
            "throttle_raw=",
            "throttle_reasons=",
        ] {
            assert!(out.contains(key), "missing line for {key}");
        }
        for &(name, _) in THRESHOLD_KINDS.iter() {
            let key = format!("threshold.{name}=");
            assert!(out.contains(&key), "missing line for {key}");
        }
        for &(id, name) in FIELD_IDS.iter() {
            let key = format!("field.{id}.{name}=");
            assert!(out.contains(&key), "missing line for {key}");
        }
        // Every input is absent, so neither derived margin can be computed.
        assert!(!out.contains("derived.margin"));
    }

    #[test]
    fn format_probe_renders_sample_variants() {
        let mut snap = absent_snapshot();
        snap.fields = vec![
            (185, "POWER_AVERAGE", Ok("U32(579310)".to_string())),
            (186, "POWER_INSTANT", Ok("F64(583.2)".to_string())),
            (82, "MEMORY_TEMP", Ok("U64(0)".to_string())),
            (193, "SHUTDOWN_TLIMIT", absent()),
            (194, "SLOWDOWN_TLIMIT", absent()),
            (195, "MEM_MAX_TLIMIT", absent()),
            (196, "GPU_MAX_TLIMIT", Ok("I64(-3)".to_string())),
        ];

        let out = format_probe(&snap);
        for variant in ["U32(579310)", "F64(583.2)", "U64(0)", "I64(-3)"] {
            assert!(out.contains(variant), "missing rendered variant {variant}");
        }
        // GPU_MAX_TLIMIT (field 196) is I64(-3): the derived margin line
        // should surface that negative headroom directly.
        assert!(out.contains("derived.margin_via_field196_c=-3"));
    }

    // -- Wave 3: NVML power, thermal margin, throttle --------------------

    #[test]
    fn sample_to_f64_handles_all_variants() {
        assert_eq!(sample_to_f64(SampleValue::F64(1.5)), 1.5);
        assert_eq!(sample_to_f64(SampleValue::U32(42)), 42.0);
        assert_eq!(sample_to_f64(SampleValue::U64(99)), 99.0);
        assert_eq!(sample_to_f64(SampleValue::I64(-3)), -3.0);
    }

    #[test]
    fn throttle_flags_maps_each_bit() {
        assert_eq!(
            throttle_flags(ThrottleReasons::SW_THERMAL_SLOWDOWN),
            ThrottleFlags {
                sw_thermal: true,
                hw_thermal: false,
                hw_power_brake: false,
                sw_power_cap: false,
            }
        );
        assert_eq!(
            throttle_flags(ThrottleReasons::HW_THERMAL_SLOWDOWN),
            ThrottleFlags {
                sw_thermal: false,
                hw_thermal: true,
                hw_power_brake: false,
                sw_power_cap: false,
            }
        );
        assert_eq!(
            throttle_flags(ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN),
            ThrottleFlags {
                sw_thermal: false,
                hw_thermal: false,
                hw_power_brake: true,
                sw_power_cap: false,
            }
        );
        assert_eq!(
            throttle_flags(ThrottleReasons::SW_POWER_CAP),
            ThrottleFlags {
                sw_thermal: false,
                hw_thermal: false,
                hw_power_brake: false,
                sw_power_cap: true,
            }
        );

        // Untracked bits (GPU_IDLE, SYNC_BOOST) must not leak into any
        // tracked flag.
        let untracked = ThrottleReasons::GPU_IDLE | ThrottleReasons::SYNC_BOOST;
        assert_eq!(throttle_flags(untracked), ThrottleFlags::default());
    }

    #[test]
    fn throttle_flags_any_of_matches_configured_reasons() {
        use crate::config::ThrottleReason;

        let flags = throttle_flags(ThrottleReasons::SW_POWER_CAP);
        // sw_power_cap is set on the raw flags, but not in the configured
        // reason list -> any_of must be false.
        assert!(!flags.any_of(&[ThrottleReason::SwThermal, ThrottleReason::HwThermal]));
        assert!(flags.any_of(&[ThrottleReason::SwPowerCap]));
    }

    #[test]
    fn power_limit_refresh_cadence() {
        assert!(!should_reprobe(true, 59, POWER_LIMIT_REFRESH_TICKS));
        assert!(should_reprobe(true, 60, POWER_LIMIT_REFRESH_TICKS));
    }

    #[test]
    fn gpu_caps_default_is_all_unavailable() {
        let caps = GpuCaps::default();
        assert_eq!(
            caps,
            GpuCaps {
                power: false,
                margin: MarginSource::Unavailable,
                mem_temp: false,
                throttle: false,
                power_limit_w: None,
            }
        );
    }

    #[test]
    fn margin_from_threshold_computes_headroom() {
        assert_eq!(margin_from_threshold(90.0, Some(39.0)), Some(51.0));
        assert_eq!(margin_from_threshold(90.0, None), None);
    }
}
