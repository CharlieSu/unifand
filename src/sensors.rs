use anyhow::{Context, Result};
use nvml_wrapper::enum_wrappers::device::{TemperatureSensor, TemperatureThreshold};
use nvml_wrapper::structs::device::FieldId;
use nvml_wrapper::sys_exports::field_id;
use nvml_wrapper::Nvml;
use std::path::{Path, PathBuf};

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

pub struct GpuSensor {
    nvml: Nvml,
}

impl GpuSensor {
    /// None when libnvidia-ml.so is absent (no nvidia runtime class) or no GPU.
    pub fn init() -> Option<GpuSensor> {
        let nvml = Nvml::init().ok()?;
        nvml.device_by_index(0).ok()?;
        Some(GpuSensor { nvml })
    }

    pub fn read_c(&self) -> Result<f64> {
        let dev = self.nvml.device_by_index(0).context("nvml device 0")?;
        let t = dev
            .temperature(TemperatureSensor::Gpu)
            .context("nvml temperature")?;
        Ok(t as f64)
    }

    /// Read-only capability/diagnostic dump for `--probe-gpu`. Never panics
    /// and never returns `Err`; every individual NVML failure becomes an
    /// `Err(String)` in place so the struct is pure data.
    pub fn probe_snapshot(&self) -> ProbeSnapshot {
        let nvml_version = self.nvml.sys_nvml_version().map_err(|e| e.to_string());
        let driver_version = self.nvml.sys_driver_version().map_err(|e| e.to_string());

        let dev = match self.nvml.device_by_index(0) {
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
            Ok(samples) => FIELD_IDS
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
                .collect(),
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
}
