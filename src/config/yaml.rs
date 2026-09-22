//! Load and save the `fanctl` config as YAML.

use std::path::Path;

use super::types::Config;
use crate::error::Error;

pub fn load(path: &Path) -> Result<Config, Error> {
    let s = std::fs::read_to_string(path)
        .map_err(|e| Error::Msg(format!("reading {}: {e}", path.display())))?;
    Ok(serde_yaml::from_str(&s)?)
}

pub fn save(path: &Path, config: &Config) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Msg(format!("creating {}: {e}", parent.display())))?;
        }
    }
    let s = serde_yaml::to_string(config)?;
    std::fs::write(path, s).map_err(|e| Error::Msg(format!("writing {}: {e}", path.display())))?;
    Ok(())
}

/// Save the config *atomically*: the YAML is written to a temporary file in
/// the same directory and then renamed over the current one, so a crash (or
/// a power cut) mid-write can never leave a half-written config behind.
pub fn save_atomic(path: &Path, config: &Config) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Msg(format!("creating {}: {e}", parent.display())))?;
        }
    }
    let s = serde_yaml::to_string(config)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| "fanctl.yaml");
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, s).map_err(|e| Error::Msg(format!("writing {}: {e}", path.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| Error::Msg(format!("saving {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{
        Aggregation, Config, ControlKind, InitialMode, Pwm, TempRef, TempSensor,
    };
    use crate::curve::CurvePoint;

    fn sample() -> Config {
        Config {
            refresh_secs: 2.0,
            pwm: vec![Pwm {
                id: "pwm1".into(),
                hwmon: "it8792".into(),
                name: Some("Fan 1".into()),
                pwm_max: 255,
                temp_sensors: vec![TempRef {
                    hwmon: "it8792".into(),
                    sensor: "temp3".into(),
                }],
                temp_sensor: None,
                aggregation: Aggregation::Max,
                control: ControlKind::Curve,
                curve: vec![
                    CurvePoint::new(30.0, 0.0),
                    CurvePoint::new(45.0, 40.0),
                    CurvePoint::new(60.0, 75.0),
                    CurvePoint::new(75.0, 100.0),
                ],
                default: InitialMode::Auto,
                duty_min: None,
                duty_max: None,
            }],
            temp_sensors: vec![TempSensor {
                hwmon: "acpitz".into(),
                sensors: vec!["temp1".into()],
            }],
            show_lm_sensors: false,
        }
    }

    #[test]
    fn legacy_single_sensor_still_parses() {
        let s = "pwm:\n  - id: pwm1\n    hwmon: it8792\n    temp_sensor: { hwmon: k10temp, sensor: temp1 }\n";
        let cfg: Config = serde_yaml::from_str(s).unwrap();
        // The legacy `temp_sensor` object is merged into the driving list.
        assert_eq!(cfg.pwm[0].driving_sensors().len(), 1);
        assert_eq!(cfg.pwm[0].temp_sensors.len(), 0);
        assert!(cfg.pwm[0].temp_sensor.is_some());
    }

    #[test]
    fn nvidia_sensor_is_referencable() {
        let s = "pwm:\n  - id: pwm1\n    hwmon: it8792\n    temp_sensors: [{ hwmon: nvidia, sensor: gpu0 }]\n    aggregation: avg\n";
        let cfg: Config = serde_yaml::from_str(s).unwrap();
        assert_eq!(cfg.pwm[0].aggregation, Aggregation::Avg);
        assert_eq!(cfg.pwm[0].driving_sensors()[0].hwmon, "nvidia");
        assert_eq!(cfg.pwm[0].driving_sensors()[0].sensor, "gpu0");
    }

    #[test]
    fn round_trips() {
        let cfg = sample();
        let s = serde_yaml::to_string(&cfg).unwrap();
        // The curve must serialize as a list of `{temp, duty}` maps.
        assert!(s.contains("curve"), "config:\n{s}");
        assert!(s.contains("temp: 30.0"), "config:\n{s}");
        assert!(s.contains("duty: 40.0"), "config:\n{s}");
        let back: Config = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn save_atomic_replaces_the_file_and_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fanctl.yaml");
        let cfg = sample();
        save(&path, &cfg).unwrap();

        // Change the curve, then save atomically.
        let mut cfg2 = cfg.clone();
        cfg2.pwm[0].curve = vec![CurvePoint::new(20.0, 10.0), CurvePoint::new(50.0, 90.0)];
        cfg2.pwm[0].default = InitialMode::Off;
        save_atomic(&path, &cfg2).unwrap();

        // The file holds exactly the new config, and it reloads cleanly.
        let back = load(&path).unwrap();
        assert_eq!(back, cfg2);
        assert!(
            !dir.path().read_dir().unwrap().any(|e| e
                .as_ref()
                .ok()
                .map(|e| e.file_name().to_string_lossy().contains(".tmp"))
                .unwrap_or(false)),
            "a temp file must not be left behind"
        );
    }

    #[test]
    fn parses_curve_points() {
        let s = "pwm:\n  - id: pwm1\n    hwmon: it8792\n    temp_sensor: { hwmon: it8792, sensor: temp1 }\n    curve:\n      - { temp: 30, duty: 0 }\n      - { temp: 75, duty: 100 }\n";
        let cfg: Config = serde_yaml::from_str(s).unwrap();
        assert_eq!(
            cfg.pwm[0].curve,
            vec![CurvePoint::new(30.0, 0.0), CurvePoint::new(75.0, 100.0)]
        );
    }

    #[test]
    fn duty_range_round_trips_and_is_optional() {
        let mut cfg = sample();
        cfg.pwm[0].duty_min = Some(20.0);
        cfg.pwm[0].duty_max = Some(80.0);
        let s = serde_yaml::to_string(&cfg).unwrap();
        assert!(s.contains("duty_min: 20.0"), "config:\n{s}");
        assert!(s.contains("duty_max: 80.0"), "config:\n{s}");
        let back: Config = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, cfg);

        // Unmeasured: the fields are omitted entirely and default to none.
        let mut cfg2 = sample();
        cfg2.pwm[0].duty_min = None;
        cfg2.pwm[0].duty_max = None;
        let s2 = serde_yaml::to_string(&cfg2).unwrap();
        assert!(!s2.contains("duty_min"), "config:\n{s2}");
        let back2: Config = serde_yaml::from_str(&s2).unwrap();
        assert_eq!(back2, cfg2);
    }

    #[test]
    fn parsels_legacy_curve_sequence_points() {
        // Pre-1.1.0 configs wrote curve points as `[temp, duty]` sequences;
        // the lenient deserializer must still parse them.
        let s = "pwm:\n  - id: pwm1\n    hwmon: it8792\n    temp_sensor: { hwmon: it8792, sensor: temp1 }\n    curve:\n      - [30, 0]\n      - [75, 100]\n";
        let cfg: Config = serde_yaml::from_str(s).unwrap();
        assert_eq!(
            cfg.pwm[0].curve,
            vec![CurvePoint::new(30.0, 0.0), CurvePoint::new(75.0, 100.0)]
        );
    }
}
