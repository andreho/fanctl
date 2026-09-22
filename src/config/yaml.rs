//! Load and save the `fanctl` config as YAML.

use std::path::Path;

use super::types::Config;
use crate::error::Error;

pub fn load(path: &Path) -> Result<Config, Error> {
    let s = std::fs::read_to_string(path).map_err(|e| Error::Msg(format!("reading {}: {e}", path.display())))?;
    Ok(serde_yaml::from_str(&s)?)
}

pub fn save(path: &Path, config: &Config) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Msg(format!("creating {}: {e}", parent.display())))?;
        }
    }
    let s = serde_yaml::to_string(config)?;
    std::fs::write(path, s).map_err(|e| Error::Msg(format!("writing {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::config::types::{Aggregation, Config, ControlKind, InitialMode, Pwm, TempRef, TempSensor};

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
                curve: vec![[30.0, 0.0], [45.0, 40.0], [60.0, 75.0], [75.0, 100.0]],
                default: InitialMode::Auto,
            }],
            temp_sensors: vec![TempSensor { hwmon: "acpitz".into(), sensors: vec!["temp1".into()] }],
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
        // The curve must serialize as a 2-element sequence.
        assert!(s.contains("curve"), "config:\n{s}");
        let back: Config = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn parses_curve_points() {
        let s = "pwm:\n  - id: pwm1\n    hwmon: it8792\n    temp_sensor: { hwmon: it8792, sensor: temp1 }\n    curve: [[30, 0], [75, 100]]\n";
        let cfg: Config = serde_yaml::from_str(s).unwrap();
        assert_eq!(cfg.pwm[0].curve, vec![[30.0, 0.0], [75.0, 100.0]]);
    }
}
