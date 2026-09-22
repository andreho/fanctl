//! A unified, source-agnostic temperature reading, plus the readers that
//! populate it: hwmon sysfs, NVIDIA GPUs (NVML, with an `nvidia-smi` CLI
//! fallback), and the lm-sensors `sensors -j` subprocess.

/// A single temperature reading, in degrees Celsius, with a display label.
#[derive(Debug, Clone, PartialEq)]
pub struct TempReading {
    /// A stable identifier, e.g. `it8792:temp3` or `lm:acpitz-acpi-0:temp1`.
    pub source: String,
    /// A human label, e.g. `Tctl` or `Composite`.
    pub label: String,
    pub celsius: f64,
}

/// Read a single hwmon temperature sensor.
pub fn read_hwmon(
    chip: &crate::hwmon::Hwmon,
    sensor: &str,
) -> Option<TempReading> {
    let value = chip.read_i64(&format!("{sensor}_input"))?;
    let label = chip
        .read(&format!("{sensor}_label"))
        .unwrap_or_else(|| sensor.to_string());
    Some(TempReading {
        source: format!("{}:{sensor}", chip.name),
        label,
        celsius: value as f64 / 1000.0,
    })
}

/// Read a temperature sensor from the `sensors -j` (lm-sensors) output.
#[allow(dead_code)]
pub fn read_lm_sensors(
    json: &str,
    chip: &str,
    sensor: &str,
) -> Option<TempReading> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let chip_obj = v.get(chip)?;
    let sensor_obj = chip_obj.get(sensor)?;
    let input = sensor_obj.get("input")?.as_f64()?;
    Some(TempReading {
        source: format!("lm:{chip}:{sensor}"),
        label: sensor.to_string(),
        celsius: input,
    })
}

/// Run `sensors -j` and return the raw JSON string (for display purposes).
pub fn read_sensors_json() -> Option<String> {
    let out = std::process::Command::new("sensors")
        .arg("-j")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// A single NVIDIA GPU.
#[derive(Debug, Clone, PartialEq)]
pub struct NvidiaGpu {
    pub index: u32,
    pub name: String,
    pub temp_c: Option<f64>,
    pub fan_percent: Option<f64>,
}

/// A process-wide, lazily-initialised NVML handle. NVML is loaded at runtime
/// (dlopen of `libnvidia-ml`), so this simply fails on machines without an
/// NVIDIA driver instead of crashing; the caller then falls back to
/// `nvidia-smi`.
fn nvml() -> Option<&'static nvml_wrapper::Nvml> {
    use std::sync::OnceLock;
    static NVML: OnceLock<Option<nvml_wrapper::Nvml>> = OnceLock::new();
    // `None` is cached permanently once init has failed (i.e. no driver).
    NVML.get_or_init(|| nvml_wrapper::Nvml::init().ok()).as_ref()
}

/// Read all NVIDIA GPUs. Primary path: NVML directly (no subprocess). If the
/// NVML library is absent or a query fails, fall back to the `nvidia-smi`
/// CLI. Returns an empty vec when neither source is available.
pub fn read_nvidia() -> Vec<NvidiaGpu> {
    if let Some(gpus) = read_nvml() {
        return gpus;
    }
    read_nvidia_smi()
}

/// Read every GPU through a live NVML handle.
fn read_nvml() -> Option<Vec<NvidiaGpu>> {
    use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
    let nvml = nvml()?;
    let count = nvml.device_count().ok()?;
    let mut out = Vec::new();
    for i in 0..count {
        let Ok(device) = nvml.device_by_index(i) else {
            continue;
        };
        let Some(name) = device.name().ok() else {
            continue;
        };
        let temp_c = device
            .temperature(TemperatureSensor::Gpu)
            .ok()
            .map(|t| t as f64);
        // Some GPUs (e.g. passive datacenter cards) expose no fan at all.
        let fan_percent = (|| {
            let n = device.num_fans().ok()?;
            (n > 0)
                .then(|| device.fan_speed(0))
                .and_then(Result::ok)
                .map(|p| p as f64)
        })();
        out.push(NvidiaGpu {
            index: i,
            name,
            temp_c,
            fan_percent,
        });
    }
    Some(out)
}

/// Run `nvidia-smi` (fallback when NVML itself is unavailable) and return a
/// reading for each GPU. Returns an empty vec when the tool is absent or
/// reports an error.
fn read_nvidia_smi() -> Vec<NvidiaGpu> {
    let out = match std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,name,temperature.gpu,fan.speed"])
        .arg("--format=csv,noheader")
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_nvidia(&String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse the CSV produced by
/// `nvidia-smi --query-gpu=index,name,temperature.gpu,fan.speed --format=csv,noheader`.
///
/// Fields may be a bare number (`45`), a percentage (`99 %`), or `[N/A]`.
pub fn parse_nvidia(csv: &str) -> Vec<NvidiaGpu> {
    let mut out = Vec::new();
    for line in csv.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if f.len() < 4 {
            continue;
        }
        let index = f[0].parse().unwrap_or(0);
        out.push(NvidiaGpu {
            index,
            name: f[1].to_string(),
            temp_c: parse_number(f[2]),
            fan_percent: parse_percent(f[3]),
        });
    }
    out
}

/// Parse a bare numeric field (e.g. `45`) or return `None` for `[N/A]`.
fn parse_number(s: &str) -> Option<f64> {
    s.parse().ok()
}

/// Parse a percentage field that may be `75 %`, `0 %`, or `[N/A]`.
fn parse_percent(s: &str) -> Option<f64> {
    let s = s.trim();
    if s == "[N/A]" || s.is_empty() {
        return None;
    }
    s.trim_end_matches('%').trim().parse().ok()
}

/// Resolve a sensor reference to a temperature in °C. The reference is either
/// a `hwmon` sensor (`hwmon_name` = chip name or `hwmonN`) or an NVIDIA GPU
/// (`hwmon_name` == `"nvidia"`, `sensor` == `"gpu<index>"`).
pub fn read_sensor(
    chips: &[crate::hwmon::Hwmon],
    nvidia: &[NvidiaGpu],
    hwmon_name: &str,
    sensor: &str,
) -> Option<f64> {
    if hwmon_name == "nvidia" {
        let idx = sensor
            .strip_prefix("gpu")
            .and_then(|s| s.parse::<usize>().ok())?;
        return nvidia.get(idx).and_then(|g| g.temp_c);
    }
    let chip = crate::hwmon::resolve(chips, hwmon_name)?;
    chip.read_i64(&format!("{sensor}_input"))
        .map(|v| v as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::Hwmon;
    #[test]
    fn parses_nvidia_csv() {
        let csv = "0, NVIDIA CMP 170HX, 77, [N/A]\n1, NVIDIA GeForce RTX 3080 Ti, 34, 0 %";
        let g = parse_nvidia(csv);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].index, 0);
        assert_eq!(g[0].temp_c, Some(77.0));
        assert_eq!(g[0].fan_percent, None, "[N/A] -> None");
        assert_eq!(g[1].temp_c, Some(34.0));
        assert_eq!(g[1].fan_percent, Some(0.0));
    }

    #[test]
    fn parses_empty_nvidia() {
        assert!(parse_nvidia("").is_empty());
    }

    #[test]
    fn read_sensor_nvidia() {
        let nvidia = vec![NvidiaGpu {
            index: 0,
            name: "GPU0".into(),
            temp_c: Some(90.0),
            fan_percent: None,
        }];
        assert_eq!(
            read_sensor(&[], &nvidia, "nvidia", "gpu0"),
            Some(90.0)
        );
        // Out-of-range index -> None.
        assert_eq!(read_sensor(&[], &nvidia, "nvidia", "gpu5"), None);
    }

    #[test]
    fn read_sensor_hwmon() {
        let dir = tempfile::tempdir().unwrap();
        let chip = dir.path().join("hwmon0");
        std::fs::create_dir_all(&chip).unwrap();
        std::fs::write(chip.join("name"), "k10temp\n").unwrap();
        std::fs::write(chip.join("temp1_input"), "42000\n").unwrap();
        std::fs::write(chip.join("temp1_label"), "Tctl\n").unwrap();
        let chip = Hwmon {
            name: "k10temp".into(),
            dir: "hwmon0".into(),
            path: chip,
        };
        assert_eq!(
            read_sensor(&[chip.clone()], &[], "k10temp", "temp1"),
            Some(42.0)
        );
        assert_eq!(read_sensor(&[chip], &[], "missing", "temp1"), None);
    }
}
