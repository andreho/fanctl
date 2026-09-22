//! The `fanctl` configuration model.
//!
//! The config describes, per fan (PWM channel): which temperature it follows,
//! the control strategy (fanctl-driven curve vs. the kernel's own auto
//! algorithm), the duty curve, and an initial mode. It also lists the
//! temperature sensors the TUI should display.

use serde::{Deserialize, Serialize};

/// How to combine the readings of several sensors into a single driving value
/// for a fan's curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Aggregation {
    /// Use the hottest reading (default — safe for "cool if CPU *or* GPU is hot").
    #[default]
    Max,
    /// Use the average of the readings.
    Avg,
    /// Use the coolest reading.
    Min,
}

/// The control strategy a fan is driven by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlKind {
    /// `fanctl` computes the duty from the curve and writes it to the PWM every
    /// tick (the PWM runs in manual mode).
    #[default]
    Curve,
    /// Let the chip's own auto-algorithm drive the PWM (`pwm_enable = 3`, or
    /// `4` for full). The curve is ignored for control, but may still be shown.
    KernelAuto,
}

/// The initial mode a fan starts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InitialMode {
    /// Follow the configured strategy (curve-following or kernel-auto).
    #[default]
    Auto,
    /// The PWM is switched off.
    Off,
    /// The PWM runs at full speed.
    Full,
}

/// A reference to a temperature sensor on a given chip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TempRef {
    /// The hwmon chip name (e.g. `it8792`) or `hwmonN`.
    pub hwmon: String,
    /// The sensor name (e.g. `temp3`).
    pub sensor: String,
}

impl TempRef {
    #[allow(dead_code)]
    pub fn full_name(&self) -> String {
        format!("{}:{}", self.hwmon, self.sensor)
    }
}

/// A single fan (a PWM channel on a chip).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pwm {
    /// The PWM channel, e.g. `pwm1`.
    pub id: String,
    /// The hwmon chip that owns this PWM (e.g. `it8792`) or `hwmonN`.
    pub hwmon: String,
    /// A human-friendly label shown in the TUI. Defaults to the PWM id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// The maximum value the PWM accepts (default 255).
    #[serde(default = "default_pwm_max")]
    pub pwm_max: u32,

    /// The temperature sensors that drive this fan's curve. One or more;
    /// their readings are combined with [`Aggregation`] into a single value
    /// that is mapped through the curve. A sensor is a [`TempRef`] — use
    /// `hwmon: "nvidia"` and `sensor: "gpu<N>"` for an NVIDIA GPU.
    #[serde(default)]
    pub temp_sensors: Vec<TempRef>,

    /// Legacy single-sensor form, accepted for older config files. Equivalent to
    /// a one-element `temp_sensors` list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_sensor: Option<TempRef>,

    /// How the `temp_sensors` readings are combined (default: `max`).
    #[serde(default)]
    pub aggregation: Aggregation,

    /// How the fan is driven.
    #[serde(default)]
    pub control: ControlKind,

    /// The duty curve as a list of `[temp °C, duty %]` pairs.
    #[serde(default)]
    pub curve: Vec<[f64; 2]>,

    /// The initial mode.
    #[serde(default)]
    pub default: InitialMode,
}

fn default_pwm_max() -> u32 {
    255
}

impl Pwm {
    /// The numeric PWM index (`pwm1` -> `1`).
    pub fn index(&self) -> Option<u32> {
        self.id
            .strip_prefix("pwm")
            .and_then(|s| s.parse().ok())
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// The sensors that drive this fan's curve, merging the (legacy)
    /// single-sensor field with the multi-sensor list.
    pub fn driving_sensors(&self) -> Vec<TempRef> {
        let mut v = self.temp_sensors.clone();
        if let Some(s) = &self.temp_sensor {
            v.push(s.clone());
        }
        v
    }
}

/// A group of temperature sensors on one chip, for display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TempSensor {
    /// The hwmon chip name (or `hwmonN`).
    pub hwmon: String,
    /// The sensor names to display (e.g. `["temp1", "temp3"]`).
    pub sensors: Vec<String>,
}

impl Default for TempSensor {
    fn default() -> Self {
        Self {
            hwmon: String::new(),
            sensors: Vec::new(),
        }
    }
}

/// The full configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// How often to refresh temperatures / re-evaluate curves, in seconds.
    #[serde(default = "default_refresh")]
    pub refresh_secs: f64,

    /// The fans (PWM channels) to control.
    #[serde(default)]
    pub pwm: Vec<Pwm>,

    /// The temperature sensors to display.
    #[serde(default)]
    pub temp_sensors: Vec<TempSensor>,

    /// When true, also pull the lm-sensors `sensors -j` output in for
    /// display (in addition to the hwmon temps).
    #[serde(default)]
    pub show_lm_sensors: bool,
}

fn default_refresh() -> f64 {
    2.0
}
