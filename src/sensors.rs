use anyhow::{Context, Result};
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
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
}
