//! TUI state and the main loop: reads temperatures, applies each fan's
//! requested mode (curve-following or kernel auto), reads back state, renders,
//! and handles input.
//!
//! A fan's curve may be driven by a *set* of temperature sensors (CPU, GPU,
//! thermistors) combined with an aggregation, so a single fan can react to,
//! say, the hottest of several sources.

use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::DefaultTerminal;

use crate::config::types::{Aggregation, Config, ControlKind, InitialMode, TempRef};
use crate::curve::Curve;
use crate::hwmon;
use crate::hwmon::Hwmon;
use crate::temps;

/// The control strategy a fan is currently driven by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Follow the configured strategy (curve or kernel auto).
    Auto,
    /// The PWM is disabled.
    Off,
    /// The PWM runs at full speed.
    Full,
    /// A fixed manual duty, 0..=100 (%).
    Manual(u8),
}

impl Mode {
    pub fn label(&self) -> String {
        match self {
            Mode::Auto => "auto".into(),
            Mode::Off => "off".into(),
            Mode::Full => "full".into(),
            Mode::Manual(n) => format!("{n}%"),
        }
    }
}

impl From<InitialMode> for Mode {
    fn from(m: InitialMode) -> Self {
        match m {
            InitialMode::Auto => Mode::Auto,
            InitialMode::Off => Mode::Off,
            InitialMode::Full => Mode::Full,
        }
    }
}

/// A single temperature reading for display.
#[derive(Debug, Clone)]
pub struct Temp {
    /// Stable id: `chipDir:sensor` (hwmon), `nvidia:gpu<N>`, or `lm:chip:sensor`.
    pub refname: String,
    /// Human label (the sensor's `_label`, the GPU name, …).
    pub label: String,
    /// `"hwmon"`, `"nvidia"`, or `"lm"`.
    pub source: String,
    pub celsius: Option<f64>,
}

/// A single fan (one PWM channel) and its runtime state.
#[derive(Debug, Clone)]
pub struct Fan {
    pub label: String,
    /// Chip that owns the PWM (for writing).
    pub chip: Option<Hwmon>,
    /// The sensors whose readings drive this fan's curve.
    pub sensors: Vec<TempRef>,
    /// How the sensor readings are combined into a single driving value.
    pub aggregation: Aggregation,
    pub pwm_max: u32,
    pub control: ControlKind,
    pub curve: Curve,
    pub index: Option<u32>,
    pub mode: Mode,
    /// Last duty percentage (read back from the chip).
    pub duty: Option<f64>,
    pub rpm: Option<u32>,
    pub err: Option<String>,
}

/// Top-level TUI state.
#[derive(Debug)]
pub struct App {
    pub config: Config,
    pub fans: Vec<Fan>,
    pub temps: Vec<Temp>,
    pub selected: usize,
    pub sorting: bool,
    pub scroll: usize,
    pub show_help: bool,
    pub status: String,
    pub quit: bool,
}

impl App {
    pub fn new(config: Config) -> Self {
        Self::from(config, Path::new(hwmon::SYS_HWMON))
    }

    /// Build the TUI state, reading hwmon from `base` (the `/sys/class/hwmon`
    /// tree in production, a fixture in tests).
    pub fn from(config: Config, base: &Path) -> Self {
        let chips = hwmon::discover(base);
        let mut app = Self {
            config: config.clone(),
            fans: Vec::new(),
            temps: Vec::new(),
            selected: 0,
            sorting: false,
            scroll: 0,
            show_help: false,
            status: String::new(),
            quit: false,
        };

        // Display: the hwmon temperature sensors named in the config.
        for ts in &config.temp_sensors {
            let Some(chip) = hwmon::resolve(&chips, &ts.hwmon) else {
                continue;
            };
            for sensor in &ts.sensors {
                let label = chip
                    .read(&format!("{sensor}_label"))
                    .unwrap_or_else(|| sensor.clone());
                app.temps.push(Temp {
                    refname: format!("{}:{sensor}", chip.dir),
                    label,
                    source: "hwmon".into(),
                    celsius: None,
                });
            }
        }
        // Display: any lm-sensors readings (opt-in) and any NVIDIA GPUs.
        if config.show_lm_sensors {
            app.add_lm_sensors();
        }
        app.add_nvidia();

        // Fans.
        for p in &config.pwm {
            app.fans.push(Fan {
                label: p.display_name().into(),
                chip: hwmon::resolve(&chips, &p.hwmon).cloned(),
                sensors: p.driving_sensors(),
                aggregation: p.aggregation,
                pwm_max: p.pwm_max,
                control: p.control,
                curve: Curve::new(p.curve.clone()),
                index: p.index(),
                mode: p.default.into(),
                duty: None,
                rpm: None,
                err: None,
            });
        }
        app
    }

    /// Pull lm-sensors `sensors -j` readings in for display.
    fn add_lm_sensors(&mut self) {
        let Some(json) = temps::read_sensors_json() else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else {
            return;
        };
        let Some(obj) = v.as_object() else {
            return;
        };
        for (chip, o) in obj {
            let Some(sensors) = o.as_object() else {
                continue;
            };
            for (sensor, so) in sensors {
                let Some(input) = so.get("input").and_then(|x| x.as_f64()) else {
                    continue;
                };
                self.temps.push(Temp {
                    refname: format!("lm:{chip}:{sensor}"),
                    label: sensor.clone(),
                    source: "lm".into(),
                    celsius: Some(input),
                });
            }
        }
    }

    /// Add any NVIDIA GPUs (from `nvidia-smi`) to the display list.
    fn add_nvidia(&mut self) {
        for g in temps::read_nvidia() {
            self.temps.push(Temp {
                refname: format!("nvidia:gpu{}", g.index),
                label: format!("GPU {} {}", g.index, g.name),
                source: "nvidia".into(),
                celsius: g.temp_c,
            });
        }
    }

    /// The main loop: refresh on a timer, handle keys, redraw.
    pub fn run(mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        let interval = Duration::from_secs_f64(self.config.refresh_secs.max(0.2));
        let mut last = Instant::now() - interval; // force an immediate first update

        loop {
            let wait = interval.saturating_sub(last.elapsed());
            if let Ok(true) = event::poll(wait) {
                if let Ok(Event::Key(k)) = event::read() {
                    self.on_key(k);
                }
                while let Ok(true) = event::poll(Duration::ZERO) {
                    if let Ok(Event::Key(k)) = event::read() {
                        self.on_key(k);
                    }
                }
            }

            if self.quit {
                return Ok(());
            }

            if last.elapsed() >= interval {
                self.do_update();
                last = Instant::now();
            }

            terminal.draw(|f| self.draw(f))?;
        }
    }

    /// Read all temperatures, apply each fan's mode, and read back state.
    fn do_update(&mut self) {
        let chips = hwmon::discover(Path::new(hwmon::SYS_HWMON));
        let nvidia = if self.uses_nvidia() {
            temps::read_nvidia()
        } else {
            Vec::new()
        };

        self.read_temps(&chips, &nvidia);

        let mut last_err: Option<String> = None;
        for f in &mut self.fans {
            // Copy the (clonable) inputs out to avoid borrow conflicts.
            let (chip, idx) = match (f.chip.clone(), f.index) {
                (Some(c), Some(i)) => (c, i),
                _ => {
                    f.err = Some(format!("no resolvable chip for {}", f.label));
                    last_err = f.err.clone();
                    continue;
                }
            };

            let duty: Option<f64> = match f.mode {
                Mode::Auto => match f.control {
                    ControlKind::Curve => {
                        let Some(tempc) = f.driving_value(&chips, &nvidia) else {
                            break;
                        };
                        let pct = f.curve.evaluate(tempc);
                        let r = chip
                            .set_pwm_manual(idx, pct, f.pwm_max)
                            .err()
                            .map(|e| e.to_string());
                        if let Some(e) = &r {
                            f.err = Some(format!("{}: {e}", f.label));
                        }
                        last_err = f.err.clone();
                        Some(pct)
                    }
                    ControlKind::KernelAuto => {
                        let r = chip.set_pwm_auto(idx).err().map(|e| e.to_string());
                        f.err = r.as_ref().map(|e| format!("{}: {e}", f.label));
                        last_err = f.err.clone();
                        None
                    }
                },
                Mode::Off => {
                    let r = chip.set_pwm_off(idx).err().map(|e| e.to_string());
                    f.err = r.as_ref().map(|e| format!("{}: {e}", f.label));
                    last_err = f.err.clone();
                    None
                }
                Mode::Full => {
                    let r = chip.set_pwm_full(idx).err().map(|e| e.to_string());
                    f.err = r.as_ref().map(|e| format!("{}: {e}", f.label));
                    last_err = f.err.clone();
                    None
                }
                Mode::Manual(n) => {
                    let r = chip
                        .set_pwm_manual(idx, n as f64, f.pwm_max)
                        .err()
                        .map(|e| e.to_string());
                    f.err = r.as_ref().map(|e| format!("{}: {e}", f.label));
                    last_err = f.err.clone();
                    Some(f64::from(n))
                }
            };
            if let Some(d) = duty {
                f.duty = Some(d);
            }
        }
        self.status = last_err.unwrap_or_default();

        // Read back the current duty and RPM for every fan.
        for f in &mut self.fans {
            let Some(chip) = f.chip.as_ref() else {
                continue;
            };
            let Some(idx) = f.index else {
                continue;
            };
            f.duty = chip.pwm_percent(idx, f.pwm_max);
            f.rpm = chip
                .read_i64(&format!("fan{idx}_input"))
                .map(|v| v as u32)
                .or_else(|| chip.read_i64(&format!("fan{idx}_rpm")).map(|v| v as u32));
        }
    }

    /// Update the display temperatures from live sources.
    fn read_temps(&mut self, chips: &[Hwmon], nvidia: &[temps::NvidiaGpu]) {
        let lm_json = if self.config.show_lm_sensors {
            temps::read_sensors_json()
        } else {
            None
        };
        for t in &mut self.temps {
            match t.source.as_str() {
                "hwmon" => {
                    if let Some((chip, sensor)) = t.refname.split_once(':') {
                        t.celsius = hwmon::resolve(chips, chip)
                            .and_then(|c| c.read_i64(&format!("{sensor}_input")))
                            .map(|v| v as f64 / 1000.0);
                    }
                }
                "nvidia" => {
                    let idx = t
                        .refname
                        .strip_prefix("nvidia:gpu")
                        .and_then(|s| s.parse::<usize>().ok());
                    t.celsius = idx.and_then(|i| nvidia.get(i).and_then(|g| g.temp_c));
                }
                "lm" => t.celsius = lm_json.as_ref().and_then(|j| lm_value(j, &t.refname)),
                _ => {}
            }
        }
    }

    fn uses_nvidia(&self) -> bool {
        self.temps
            .iter()
            .any(|t| t.source == "nvidia")
            || self
                .fans
                .iter()
                .any(|f| f.sensors.iter().any(|s| s.hwmon == "nvidia"))
    }

    // ---- input ----------------------------------------------------------

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Esc => self.show_help = false,

            // Fan selection.
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Tab => self.select(-1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::BackTab => self.select(1),

            // Modes for the selected fan.
            KeyCode::Char('a') | KeyCode::Char('A') => self.set_mode(Mode::Auto),
            KeyCode::Char('f') | KeyCode::Char('F') => self.set_mode(Mode::Full),
            KeyCode::Char('o') | KeyCode::Char('O') => self.set_mode(Mode::Off),
            KeyCode::Char('1') => self.set_mode(Mode::Manual(10)),
            KeyCode::Char('2') => self.set_mode(Mode::Manual(20)),
            KeyCode::Char('3') => self.set_mode(Mode::Manual(30)),
            KeyCode::Char('4') => self.set_mode(Mode::Manual(40)),
            KeyCode::Char('5') => self.set_mode(Mode::Manual(50)),
            KeyCode::Char('6') => self.set_mode(Mode::Manual(60)),
            KeyCode::Char('7') => self.set_mode(Mode::Manual(70)),
            KeyCode::Char('8') => self.set_mode(Mode::Manual(80)),
            KeyCode::Char('9') => self.set_mode(Mode::Manual(90)),
            KeyCode::Char('0') => self.set_mode(Mode::Manual(0)),

            // Sorting + scrolling.
            KeyCode::Char('s') | KeyCode::Char('S') => self.sorting = !self.sorting,
            KeyCode::PageDown => self.scroll = (self.scroll + 6).min(10000),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(6),
            KeyCode::End => self.scroll = 10000,
            KeyCode::Home => self.scroll = 0,

            _ => {}
        }
    }

    fn select(&mut self, delta: isize) {
        let n = self.fans.len();
        if n == 0 {
            return;
        }
        let n = n as isize;
        let cur = self.selected as isize;
        let next = if delta >= 0 {
            (cur + delta) % n
        } else {
            (cur + delta).rem_euclid(n)
        };
        self.selected = next as usize;
    }

    fn set_mode(&mut self, mode: Mode) {
        if let Some(f) = self.fans.get_mut(self.selected) {
            f.mode = mode;
        }
    }
}

impl Fan {
    /// The single value that drives this fan's curve: the aggregated reading
    /// of all its `sensors` (hwmon or nvidia). `None` when none is readable.
    pub fn driving_value(&self, chips: &[Hwmon], nvidia: &[temps::NvidiaGpu]) -> Option<f64> {
        let vals: Vec<f64> = self
            .sensors
            .iter()
            .filter_map(|s| temps::read_sensor(chips, nvidia, &s.hwmon, &s.sensor))
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(match self.aggregation {
            Aggregation::Max => vals.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            Aggregation::Min => vals.iter().copied().fold(f64::INFINITY, f64::min),
            Aggregation::Avg => vals.iter().sum::<f64>() / (vals.len() as f64),
        })
    }
}

/// Read an `lm:chip:sensor` value from a `sensors -j` JSON document.
fn lm_value(json: &str, refname: &str) -> Option<f64> {
    let rest = refname.strip_prefix("lm:")?;
    let (chip, sensor) = rest.split_once(':')?;
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let input = v.get(chip)?.get(sensor)?.get("input")?.as_f64()?;
    Some(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{ControlKind, InitialMode, Pwm, TempRef, TempSensor};
    use crate::curve::CurvePoint;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn config() -> Config {
        Config {
            refresh_secs: 2.0,
            pwm: vec![Pwm {
                id: "pwm1".into(),
                hwmon: "it8792".into(),
                name: Some("Fan 1".into()),
                pwm_max: 255,
                temp_sensors: vec![TempRef {
                    hwmon: "it8792".into(),
                    sensor: "temp1".into(),
                }],
                temp_sensor: None,
                aggregation: Aggregation::Max,
                control: ControlKind::Curve,
                curve: vec![CurvePoint::new(30.0, 0.0), CurvePoint::new(75.0, 100.0)],
                default: InitialMode::Auto,
            }],
            temp_sensors: vec![TempSensor {
                hwmon: "k10temp".into(),
                sensors: vec!["temp1".into()],
            }],
            show_lm_sensors: false,
        }
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn help_toggles_and_esc_closes() {
        let mut app = App::from(config(), std::path::Path::new("/nonexistent"));
        assert!(!app.show_help);
        app.on_key(key(KeyCode::Char('?')));
        assert!(app.show_help);
        app.on_key(key(KeyCode::Esc));
        assert!(!app.show_help);
    }

    #[test]
    fn q_quits_and_modes_apply_to_selected_fan() {
        let mut app = App::from(config(), std::path::Path::new("/nonexistent"));
        assert_eq!(app.fans.len(), 1);
        app.on_key(key(KeyCode::Char('f')));
        assert_eq!(app.fans[0].mode, Mode::Full);
        app.on_key(key(KeyCode::Char('5')));
        assert_eq!(app.fans[0].mode, Mode::Manual(50));
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.fans[0].mode, Mode::Auto);
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn selection_wraps_around() {
        let mut cfg = config();
        cfg.pwm = vec![
            cfg.pwm[0].clone(),
            Pwm {
                id: "pwm2".into(),
                ..cfg.pwm[0].clone()
            },
            Pwm {
                id: "pwm3".into(),
                ..cfg.pwm[0].clone()
            },
        ];
        let mut app = App::from(cfg, std::path::Path::new("/nonexistent"));
        app.selected = 0;
        app.select(1);
        assert_eq!(app.selected, 1);
        app.select(1);
        assert_eq!(app.selected, 2);
        app.select(1);
        assert_eq!(app.selected, 0, "wraps forward");
        app.select(-1);
        assert_eq!(app.selected, 2, "wraps backward");
    }

    /// A fan driven by an nvidia sensor + a hwmon sensor, combined with `max`.
    #[test]
    fn driving_value_uses_max_of_sensors() {
        let mut cfg = config();
        cfg.pwm[0].temp_sensors = vec![
            TempRef {
                hwmon: "k10temp".into(),
                sensor: "temp1".into(),
            },
            TempRef {
                hwmon: "nvidia".into(),
                sensor: "gpu0".into(),
            },
        ];
        let fan = Fan {
            label: "f".into(),
            chip: None,
            sensors: cfg.pwm[0].driving_sensors(),
            aggregation: Aggregation::Max,
            pwm_max: 255,
            control: ControlKind::Curve,
            curve: Curve::new(vec![CurvePoint::new(0.0, 0.0), CurvePoint::new(100.0, 100.0)]),
            index: None,
            mode: Mode::Auto,
            duty: None,
            rpm: None,
            err: None,
        };
        let nvidia = vec![temps::NvidiaGpu {
            index: 0,
            name: "TestGPU".into(),
            temp_c: Some(90.0),
            fan_percent: None,
        }];
        // No matching k10temp chip here, so only the nvidia reading is present.
        let v = fan.driving_value(&[], &nvidia).unwrap();
        assert!((v - 90.0).abs() < 1e-9);
    }
}
