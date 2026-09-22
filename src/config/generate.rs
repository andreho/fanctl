//! Build a default `Config` by probing the live hwmon tree, so a first run
//! produces a sensible starting point for the user to tune.

use std::path::Path;

use super::types::{Aggregation, Config, ControlKind, InitialMode, Pwm, TempRef, TempSensor};
use crate::hwmon;

/// A reasonable per-fan curve to seed auto-generated config from.
const DEFAULT_CURVE: [[f64; 2]; 4] = [[30.0, 0.0], [45.0, 40.0], [60.0, 75.0], [75.0, 100.0]];

/// Probe `base` (the hwmon tree) and produce a default config.
pub fn generate(base: &Path) -> Config {
    let chips = hwmon::discover(base);

    // How many chips share each name? Chips whose name is shared must be
    // referenced by their unique directory name instead.
    let name_count: std::collections::HashMap<&str, usize> = chips
        .iter()
        .map(|c| c.name.as_str())
        .fold(std::collections::HashMap::new(), |mut m, n| {
            *m.entry(n).or_insert(0) += 1;
            m
        });
    let ref_for = |c: &hwmon::Hwmon| -> String {
        if name_count.get(c.name.as_str()) == Some(&1) {
            c.name.clone()
        } else {
            c.dir.clone()
        }
    };

    let mut pwms: Vec<Pwm> = Vec::new();
    let mut temps: Vec<TempSensor> = Vec::new();

    for chip in &chips {
        let ref_ = ref_for(chip);

        // Show every temperature sensor that actually reports a reading.
        let mut sensors = Vec::new();
        for &tn in &chip.temp_numbers() {
            let name = format!("temp{tn}");
            if chip.read_i64(&format!("{name}_input")).is_some() {
                sensors.push(name);
            }
        }
        if !sensors.is_empty() {
            temps.push(TempSensor {
                hwmon: ref_.clone(),
                sensors,
            });
        }

        // One controllable fan per PWM channel, following the first available
        // temperature on the same chip.
        let follow = chip.temp_numbers().first().copied();
        for pn in chip.pwm_numbers() {
            let sensor = match follow {
                Some(tn) => format!("temp{tn}"),
                None => "temp1".to_string(),
            };
            pwms.push(Pwm {
                id: format!("pwm{pn}"),
                hwmon: ref_.clone(),
                name: None,
                pwm_max: 255,
                temp_sensors: vec![TempRef {
                    hwmon: ref_.clone(),
                    sensor,
                }],
                temp_sensor: None,
                aggregation: Aggregation::Max,
                control: ControlKind::Curve,
                curve: DEFAULT_CURVE.to_vec(),
                default: InitialMode::Auto,
            });
        }
    }

    Config {
        refresh_secs: 2.0,
        pwm: pwms,
        temp_sensors: temps,
        show_lm_sensors: false,
    }
}
