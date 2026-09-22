//! The `fanctld` daemon: the control engine (reads sensors, drives the fan
//! PWMs, keeps the current state) and the Unix-socket server that serves
//! `fanctlui` clients.

/// The daemon's control loop ([`run`]) and its SIGHUP pipe.
pub mod signal;

use std::io::ErrorKind;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config;
use crate::config::types::{Aggregation, Config, ControlKind, InitialMode, Pwm, TempRef};
use crate::curve::{Curve, CurvePoint};
use crate::ipc::{self, FanState, Mode, Request, Response, State, Temp};
use crate::{hwmon, sweep, temps};

/// The control engine: it holds the config and every fan's runtime state.
/// The daemon owns exactly one engine; it is re-created wholesale on a config
/// reload.
pub struct Engine {
    /// The hwmon tree (`/sys/class/hwmon` in production, a fixture in tests).
    pub base: PathBuf,
    /// The config file this engine was built from (used when reloading).
    pub config_path: PathBuf,
    pub config: Config,
    pub fans: Vec<EngineFan>,
    /// The temperature readings to show (hwmon, lm-sensors, NVIDIA).
    pub temps: Vec<Temp>,
    /// The last error across all fans, or empty when all is well.
    pub status: String,
    /// The fan being calibrated (swept) right now, if any. While this is
    /// set, `tick()` keeps the fan's last duty instead of re-applying its
    /// mode, so the sweep can own the channel.
    pub sweep: Option<SweepState>,
    /// The last finished calibration sweep (the TUI shows its note).
    pub last_sweep: Option<sweep::SweepReport>,
}

/// The state of a calibration sweep in flight.
#[derive(Debug, PartialEq)]
pub struct SweepState {
    /// The fan being swept (its `pwmN` id).
    pub fan: String,
    /// The fan's mode before the sweep; restored when the sweep finishes.
    pub prev: Mode,
    /// The (duty, rpm) steps observed so far.
    pub steps: Vec<sweep::SweepStep>,
}

/// One fan (a PWM channel) and its runtime state.
pub struct EngineFan {
    /// The fan's id (`pwm1`).
    pub id: String,
    pub label: String,
    /// The hwmon chip that owns this PWM (name or `hwmonN`).
    pub hwmon: String,
    pub pwm_max: u32,
    /// How the fan is driven when in `Mode::Auto`.
    pub control: ControlKind,
    /// The duty curve, used when the control kind is `curve`.
    pub curve: Curve,
    /// The sensors whose readings drive this fan's curve.
    pub sensors: Vec<TempRef>,
    pub aggregation: Aggregation,
    /// The mode the fan is currently driven in.
    pub mode: Mode,
    /// The chip last resolved for this fan (updated each tick).
    pub chip: Option<hwmon::Hwmon>,
    /// The numeric PWM index (`pwm1` -> `1`).
    pub index: Option<u32>,
    /// The last duty percentage (read back from the chip, or computed).
    pub duty: Option<f64>,
    /// The last tachometer reading, if the chip exposes one.
    pub rpm: Option<u32>,
    /// The last error applying the mode, if any.
    pub err: Option<String>,
    /// The fan's measured effective duty range (calibration sweep), if
    /// any: the curve's output is clamped to it.
    pub duty_min: Option<f64>,
    pub duty_max: Option<f64>,
}

impl Engine {
    /// Build an engine from a freshly loaded config.
    pub fn new(config: Config, base: PathBuf, config_path: PathBuf) -> Self {
        let chips = hwmon::discover(&base);
        let mut engine = Self {
            base,
            config_path,
            config: config.clone(),
            fans: config.pwm.iter().map(pwm_to_engine_fan).collect(),
            temps: Vec::new(),
            status: String::new(),
            sweep: None,
            last_sweep: None,
        };

        // Display: the hwmon temperature sensors named in the config.
        for ts in &engine.config.temp_sensors {
            let Some(chip) = hwmon::resolve(&chips, &ts.hwmon) else {
                continue;
            };
            for sensor in &ts.sensors {
                let label = chip
                    .read(&format!("{sensor}_label"))
                    .unwrap_or_else(|| sensor.clone());
                engine.temps.push(Temp {
                    refname: format!("{}:{sensor}", chip.dir),
                    label,
                    source: "hwmon".into(),
                    celsius: None,
                });
            }
        }
        if engine.config.show_lm_sensors {
            engine.add_lm_sensors();
        }
        engine.add_nvidia();
        engine
    }

    /// One control cycle: read all sensors, apply each fan's mode, and read
    /// the results back.
    pub fn tick(&mut self) {
        let chips = hwmon::discover(&self.base);
        let nvidia = if self.uses_nvidia() {
            temps::read_nvidia()
        } else {
            Vec::new()
        };
        let lm_json = if self.config.show_lm_sensors {
            temps::read_sensors_json()
        } else {
            None
        };

        self.update_temps(&chips, &nvidia, lm_json.as_deref());

        let mut last_err = String::new();
        for f in &mut self.fans {
            f.chip = hwmon::resolve(&chips, &f.hwmon).cloned();
            // A sweep owns this channel: keep its current duty (the
            // read-back below still refreshes the displayed values).
            if self.sweep.as_ref().is_some_and(|s| s.fan == f.id) {
                continue;
            }
            // Copy the (clonable) chip out to avoid a borrow conflict with
            // `f` below.
            let (chip, idx) = match (f.chip.clone(), f.index) {
                (Some(c), Some(i)) => (c, i),
                _ => {
                    f.err = Some(format!("no resolvable chip for {}", f.label));
                    last_err = f.err.clone().unwrap_or_default();
                    continue;
                }
            };
            Self::apply_fan(&chips, &chip, idx, f, &nvidia);
            if let Some(e) = &f.err {
                last_err = e.clone();
            }
        }
        self.status = last_err;

        // Read back the duty and the tachometer for every fan.
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

    /// Change the mode of the fan identified by `id` and apply it right away,
    /// so the next state read-back reflects it.
    pub fn set_mode(&mut self, id: &str, mode: Mode) -> Result<(), String> {
        let base = self.base.clone();
        let uses_nvidia = self.uses_nvidia();
        let Some(f) = self.fans.iter_mut().find(|f| f.id == id) else {
            return Err(format!("no fan {id}"));
        };
        // Mode changes race with a sweep in flight (both write the chip);
        // refuse them until the sweep finishes and restores the mode.
        if self.sweep.as_ref().is_some_and(|s| s.fan == id) {
            return Err(format!(
                "{}: a calibration sweep is in progress; wait for it to finish",
                f.label
            ));
        }
        f.mode = mode;
        f.err = None;
        Self::apply_one(f, &base, uses_nvidia);
        // A failed write is recorded on the fan; report it so one-shot
        // callers (and the TUI's job status) see the failure, not a
        // spurious success.
        match f.err.clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    // ---- calibration sweep -------------------------------------------------

    /// Start a calibration sweep of the fan `id`: validate everything
    /// (the fan exists, its chip is resolvable, a tachometer is readable)
    /// and remember the fan's current mode so it can be restored. The
    /// actual duty changes happen through [`Engine::sweep_write`] and
    /// friends; the sweep ends with [`Engine::sweep_finish`].
    pub fn sweep_begin(&mut self, id: &str) -> Result<(), String> {
        if let Some(s) = &self.sweep {
            return Err(format!(
                "a calibration sweep of {} is already running",
                s.fan
            ));
        }
        if self.config.pwm.iter().position(|p| p.id == id).is_none() {
            return Err(format!("no fan {id} in the config"));
        }
        let Some(f) = self.fans.iter_mut().find(|f| f.id == id) else {
            return Err(format!("no fan {id}"));
        };
        // Make sure the chip and a tachometer are actually available: a
        // sweep without a readable tachometer can only guess.
        let chips = hwmon::discover(&self.base);
        f.chip = hwmon::resolve(&chips, &f.hwmon).cloned();
        let (chip, idx) = match (f.chip.clone(), f.index) {
            (Some(c), Some(i)) => (c, i),
            _ => {
                let e = format!("no resolvable chip for {}", f.label);
                f.err = Some(e.clone());
                return Err(e);
            }
        };
        if chip.read_i64(&format!("fan{idx}_input")).is_none()
            && chip.read_i64(&format!("fan{idx}_rpm")).is_none()
        {
            let e = format!(
                "{}: no readable tachometer (fan{idx}_input) — a sweep needs one to observe the fan",
                f.label
            );
            f.err = Some(e.clone());
            return Err(e);
        }
        f.err = None;
        self.sweep = Some(SweepState {
            fan: id.into(),
            prev: f.mode,
            steps: Vec::new(),
        });
        Ok(())
    }

    /// Write the next sweep duty to the sweeping fan's chip and refresh
    /// the fan's displayed duty.
    pub fn sweep_write(&mut self, duty: u8) -> Result<(), String> {
        let Some(sweep) = self.sweep.as_ref() else {
            return Err("no sweep in progress".into());
        };
        let Some(f) = self.fans.iter_mut().find(|f| f.id == sweep.fan) else {
            return Err(format!("no fan {}", sweep.fan));
        };
        let (chip, idx) = match (f.chip.clone(), f.index) {
            (Some(c), Some(i)) => (c, i),
            _ => {
                let chips = hwmon::discover(&self.base);
                match (hwmon::resolve(&chips, &f.hwmon).cloned(), f.index) {
                    (Some(c), Some(i)) => {
                        f.chip = Some(c.clone());
                        (c, i)
                    }
                    _ => return Err(format!("no resolvable chip for {}", f.label)),
                }
            }
        };
        chip.set_pwm_manual(idx, f64::from(duty), f.pwm_max)
            .map_err(|e| format!("{}: {e}", f.label))?;
        f.err = None;
        f.duty = chip.pwm_percent(idx, f.pwm_max);
        Ok(())
    }

    /// Read the sweeping fan's tachometer (0 rpm when unreadable) and
    /// refresh the fan's displayed rpm/duty.
    pub fn sweep_read(&mut self) -> u32 {
        let Some(sweep) = self.sweep.as_ref() else {
            return 0;
        };
        let Some(f) = self.fans.iter_mut().find(|f| f.id == sweep.fan) else {
            return 0;
        };
        let Some((chip, idx)) = f.chip.clone().zip(f.index) else {
            return 0;
        };
        let raw = chip
            .read_i64(&format!("fan{idx}_input"))
            .or_else(|| chip.read_i64(&format!("fan{idx}_rpm")));
        f.rpm = raw.map(|v| v.max(0) as u32);
        f.duty = chip.pwm_percent(idx, f.pwm_max);
        raw.map(|v| v.max(0) as u32).unwrap_or(0)
    }

    /// Record one observed (duty, rpm) step of the sweep in progress.
    pub fn sweep_record(&mut self, duty: u8, rpm: u32) {
        if let Some(s) = self.sweep.as_mut() {
            s.steps.push(sweep::SweepStep { duty, rpm });
        }
    }

    /// Finish the sweep in progress: derive the fan's effective duty
    /// range, persist it to the config (when it is useful), restore the
    /// fan's pre-sweep mode, and keep the result in
    /// [`Engine::last_sweep`] for the state snapshots.
    pub fn sweep_finish(&mut self) {
        let Some(sweep_state) = self.sweep.take() else {
            return;
        };
        let (min, max, saturated) = sweep::derive(&sweep_state.steps);
        let useful = sweep::worth_saving(min, max);

        // The fan responds over a proper subrange: persist it (the next
        // curve evaluation is clamped to it) and write the config
        // atomically, rolling the in-memory state back on failure.
        let mut saved = false;
        let mut save_failed: Option<String> = None;
        if useful {
            let (min_f, max_f) = (f64::from(min.unwrap()), f64::from(max.unwrap()));
            if let Some(index) = self.config.pwm.iter().position(|p| p.id == sweep_state.fan) {
                let old = (
                    self.config.pwm[index].duty_min,
                    self.config.pwm[index].duty_max,
                );
                self.config.pwm[index].duty_min = Some(min_f);
                self.config.pwm[index].duty_max = Some(max_f);
                if let Some(f) = self.fans.iter_mut().find(|f| f.id == sweep_state.fan) {
                    f.duty_min = Some(min_f);
                    f.duty_max = Some(max_f);
                }
                match config::yaml::save_atomic(&self.config_path, &self.config) {
                    Ok(()) => saved = true,
                    Err(e) => {
                        self.config.pwm[index].duty_min = old.0;
                        self.config.pwm[index].duty_max = old.1;
                        if let Some(f) = self.fans.iter_mut().find(|f| f.id == sweep_state.fan) {
                            f.duty_min = old.0;
                            f.duty_max = old.1;
                        }
                        save_failed = Some(e.to_string());
                        self.status = format!(
                            "{}: range measured, but saving the config failed: {e}",
                            sweep_state.fan
                        );
                    }
                }
            }
        }

        // Restore the fan's pre-sweep mode right away (the next tick would
        // do it anyway, but the fan must not sit at a sweep duty for 2 s).
        let base = self.base.clone();
        let uses_nvidia = self.uses_nvidia();
        if let Some(f) = self.fans.iter_mut().find(|f| f.id == sweep_state.fan) {
            f.mode = sweep_state.prev;
            Self::apply_one(f, &base, uses_nvidia);
            f.err = None;
        }

        let note = if let Some(e) = save_failed {
            format!(
                "{}: the fan spins between {} % and {} % duty, but saving the range to the config failed: {e}",
                sweep_state.fan,
                min.unwrap(),
                max.unwrap()
            )
        } else {
            sweep::describe(
                &sweep_state.fan,
                min,
                max,
                saturated,
                &sweep_state.steps,
                saved,
            )
        };
        self.last_sweep = Some(sweep::SweepReport {
            fan: sweep_state.fan,
            min_duty: min,
            max_duty: max,
            saturated,
            note,
        });
    }

    /// Abort the sweep in progress with an error (a failed write, a config
    /// reload that dropped the fan, …): restore the fan's mode and record
    /// the reason in the status line.
    pub fn sweep_abort(&mut self, why: String) {
        let Some(sweep_state) = self.sweep.take() else {
            return;
        };
        let base = self.base.clone();
        let uses_nvidia = self.uses_nvidia();
        if let Some(f) = self.fans.iter_mut().find(|f| f.id == sweep_state.fan) {
            f.mode = sweep_state.prev;
            Self::apply_one(f, &base, uses_nvidia);
            // `why` is already labeled by the caller (`sweep_write`).
            f.err = Some(why.clone());
        }
        self.status = format!("sweep of {}: {why}", sweep_state.fan);
    }

    /// Persist a new duty curve and initial mode for the fan identified by
    /// `id` into the config file (atomically) and apply them to the running
    /// engine, so the next tick already follows the new curve.
    pub fn save_fan_config(
        &mut self,
        id: &str,
        curve: Vec<CurvePoint>,
        default_mode: InitialMode,
    ) -> Result<(), String> {
        // Validate everything before touching any state.
        if curve.is_empty() {
            return Err("the curve needs at least one point".into());
        }
        let mut prev = f64::NEG_INFINITY;
        for p in &curve {
            if !(0.0..=150.0).contains(&p.temp) {
                return Err(format!(
                    "bad curve point: {} °C is out of range (0..=150)",
                    p.temp
                ));
            }
            if !(0.0..=100.0).contains(&p.duty) {
                return Err(format!(
                    "bad curve point: {} % duty is out of range (0..=100)",
                    p.duty
                ));
            }
            if p.temp <= prev {
                return Err(format!(
                    "the curve points must be strictly increasing in temperature ({} °C)",
                    p.temp
                ));
            }
            prev = p.temp;
        }

        let index = self
            .config
            .pwm
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| format!("no fan {id} in the config"))?;
        let (old_curve, old_default) = (
            self.config.pwm[index].curve.clone(),
            self.config.pwm[index].default,
        );
        let old_runtime_curve = self
            .fans
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.curve.clone());

        self.config.pwm[index].curve = curve;
        self.config.pwm[index].default = default_mode;
        if let Some(f) = self.fans.iter_mut().find(|f| f.id == id) {
            f.curve = Curve::new(self.config.pwm[index].curve.clone());
        }

        match config::yaml::save_atomic(&self.config_path, &self.config) {
            Ok(()) => Ok(()),
            Err(e) => {
                // The write failed: roll the in-memory state back, the file
                // on disk still holds the old config.
                self.config.pwm[index].curve = old_curve;
                self.config.pwm[index].default = old_default;
                if let Some(c) = old_runtime_curve {
                    if let Some(f) = self.fans.iter_mut().find(|f| f.id == id) {
                        f.curve = c;
                    }
                }
                Err(e.to_string())
            }
        }
    }

    /// Re-read the config file and rebuild the engine from it.
    ///
    /// A rebuild would otherwise drop in-memory runtime state, so the sweep
    /// state is carried over: a sweep in flight survives the reload, and if
    /// its fan was removed from the config the sweep thread notices on its
    /// next step and aborts.
    pub fn reload(&mut self) -> Result<(), String> {
        let cfg = config::yaml::load(&self.config_path).map_err(|e| e.to_string())?;
        let sweep = self.sweep.take();
        let last_sweep = self.last_sweep.take();
        *self = Self::new(cfg, self.base.clone(), self.config_path.clone());
        self.sweep = sweep;
        self.last_sweep = last_sweep;
        self.tick();
        Ok(())
    }

    /// The state snapshot served to clients.
    pub fn snapshot(&self) -> State {
        State {
            refresh_secs: self.config.refresh_secs,
            status: self.status.clone(),
            fans: self
                .fans
                .iter()
                .map(|f| {
                    // The editor dialog edits the *config* list, so report
                    // the config's (unsorted) curve and initial mode.
                    let cfg = self.config.pwm.iter().find(|p| p.id == f.id);
                    FanState {
                        id: f.id.clone(),
                        label: f.label.clone(),
                        mode: f.mode,
                        duty: f.duty,
                        rpm: f.rpm,
                        err: f.err.clone(),
                        curve: cfg.map(|p| p.curve.clone()).unwrap_or_default(),
                        default_mode: cfg.map(|p| p.default).unwrap_or_default(),
                    }
                })
                .collect(),
            temps: self.temps.clone(),
            sweeping: self.sweep.as_ref().map(|s| s.fan.clone()),
            sweep: self.last_sweep.clone(),
        }
    }

    // ---- internals ---------------------------------------------------------

    /// Re-apply a single fan's mode right away (used by
    /// [`Engine::set_mode`); takes no `&mut self` so it can run while a fan
    /// from `self.fans` is borrowed.
    fn apply_one(f: &mut EngineFan, base: &std::path::Path, uses_nvidia: bool) {
        let chips = hwmon::discover(base);
        let nvidia = if uses_nvidia {
            temps::read_nvidia()
        } else {
            Vec::new()
        };
        f.chip = hwmon::resolve(&chips, &f.hwmon).cloned();
        let (chip, idx) = match (f.chip.clone(), f.index) {
            (Some(c), Some(i)) => (c, i),
            _ => {
                f.err = Some(format!("no resolvable chip for {}", f.label));
                return;
            }
        };
        Self::apply_fan(&chips, &chip, idx, f, &nvidia);
        f.duty = chip.pwm_percent(idx, f.pwm_max);
        f.rpm = chip
            .read_i64(&format!("fan{idx}_input"))
            .map(|v| v as u32)
            .or_else(|| chip.read_i64(&format!("fan{idx}_rpm")).map(|v| v as u32));
    }

    /// Apply `f.mode` to the fan's chip and record the resulting duty /
    /// error. `chips` is the full chip set (a fan's *sensors* may live on a
    /// different chip than the fan's PWM).
    fn apply_fan(
        chips: &[hwmon::Hwmon],
        chip: &hwmon::Hwmon,
        idx: u32,
        f: &mut EngineFan,
        nvidia: &[temps::NvidiaGpu],
    ) {
        let duty: Option<f64> = match f.mode {
            Mode::Auto => match f.control {
                ControlKind::Curve => {
                    let Some(tempc) = driving_value(&f.sensors, f.aggregation, chips, nvidia)
                    else {
                        f.err = Some(format!("{}: no reading from its sensors", f.label));
                        return;
                    };
                    let mut pct = f.curve.evaluate(tempc);
                    // A measured duty range (calibration sweep): outside it
                    // the fan does not respond, so keep the curve inside.
                    // Explicit commands are never clamped.
                    if let Some(min) = f.duty_min {
                        pct = pct.max(min);
                    }
                    if let Some(max) = f.duty_max {
                        pct = pct.min(max);
                    }
                    if let Err(e) = chip.set_pwm_manual(idx, pct, f.pwm_max) {
                        f.err = Some(format!("{}: {e}", f.label));
                    }
                    Some(pct)
                }
                ControlKind::KernelAuto => {
                    if let Err(e) = chip.set_pwm_auto(idx) {
                        f.err = Some(format!("{}: {e}", f.label));
                    }
                    None
                }
            },
            Mode::Off => {
                if let Err(e) = chip.set_pwm_off(idx, f.pwm_max) {
                    f.err = Some(format!("{}: {e}", f.label));
                }
                None
            }
            Mode::Full => {
                if let Err(e) = chip.set_pwm_full(idx, f.pwm_max) {
                    f.err = Some(format!("{}: {e}", f.label));
                }
                None
            }
            Mode::Manual { percent } => {
                if let Err(e) = chip.set_pwm_manual(idx, f64::from(percent), f.pwm_max) {
                    f.err = Some(format!("{}: {e}", f.label));
                }
                Some(f64::from(percent))
            }
        };
        if let Some(d) = duty {
            f.duty = Some(d);
        }
    }

    fn uses_nvidia(&self) -> bool {
        self.temps.iter().any(|t| t.source == "nvidia")
            || self
                .fans
                .iter()
                .any(|f| f.sensors.iter().any(|s| s.hwmon == "nvidia"))
    }

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

    fn update_temps(
        &mut self,
        chips: &[hwmon::Hwmon],
        nvidia: &[temps::NvidiaGpu],
        lm_json: Option<&str>,
    ) {
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
                "lm" => t.celsius = lm_json.and_then(|j| lm_value(j, &t.refname)),
                _ => {}
            }
        }
    }
}

/// Build an [`EngineFan`] from a config entry.
fn pwm_to_engine_fan(p: &Pwm) -> EngineFan {
    let (duty_min, duty_max) = sweep::normalize(p.duty_min, p.duty_max);
    EngineFan {
        id: p.id.clone(),
        label: p.display_name().into(),
        hwmon: p.hwmon.clone(),
        pwm_max: p.pwm_max,
        control: p.control,
        curve: Curve::new(p.curve.clone()),
        sensors: p.driving_sensors(),
        aggregation: p.aggregation,
        mode: p.default.into(),
        chip: None,
        index: p.index(),
        duty: None,
        rpm: None,
        err: None,
        duty_min,
        duty_max,
    }
}

/// The single value that drives a fan's curve: the aggregated reading of all
/// its sensors. `None` when no sensor is readable.
fn driving_value(
    sensors: &[TempRef],
    aggregation: Aggregation,
    chips: &[hwmon::Hwmon],
    nvidia: &[temps::NvidiaGpu],
) -> Option<f64> {
    let vals: Vec<f64> = sensors
        .iter()
        .filter_map(|s| temps::read_sensor(chips, nvidia, &s.hwmon, &s.sensor))
        .collect();
    if vals.is_empty() {
        return None;
    }
    Some(match aggregation {
        Aggregation::Max => vals.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        Aggregation::Min => vals.iter().copied().fold(f64::INFINITY, f64::min),
        Aggregation::Avg => vals.iter().sum::<f64>() / (vals.len() as f64),
    })
}

/// Read an `lm:chip:sensor` value out of a `sensors -j` JSON document.
fn lm_value(json: &str, refname: &str) -> Option<f64> {
    let rest = refname.strip_prefix("lm:")?;
    let (chip, sensor) = rest.split_once(':')?;
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let input = v.get(chip)?.get(sensor)?.get("input")?.as_f64()?;
    Some(input)
}

/// Serve clients on `socket` until it errors: tick the engine on the
/// configured interval, answer one request per connection, and reload the
/// config on `SIGHUP` (when a [`Sighup`] handler is provided).
/// The daemon's main loop: tick, accept clients, handle SIGHUP.
///
/// The engine is shared behind a `Mutex`, because the calibration-sweep
/// threads borrow it too — only briefly (they sleep with the lock
/// released), so the loop stays free to tick the other fans and serve
/// clients while a fan settles.
pub fn run(
    socket: &std::path::Path,
    engine: &Arc<Mutex<Engine>>,
    sighup: &Option<signal::Sighup>,
) -> std::io::Result<()> {
    std::fs::remove_file(socket).ok();
    let listener = UnixListener::bind(socket)?;
    // The default umask would leave the socket owned-and-only (group/writable
    // by the daemon's user). For a root daemon driven by a user-space TUI
    // that blocks the client, so open the socket up to all local users.
    std::fs::set_permissions(socket, std::os::unix::fs::PermissionsExt::from_mode(0o666)).ok();
    listener.set_nonblocking(true)?;
    eprintln!("fanctld: listening on {}", socket.display());

    let mut next_tick = Instant::now(); // tick immediately
    loop {
        let interval = Duration::from_secs_f64(engine.lock().unwrap().config.refresh_secs.max(0.2));

        if Instant::now() >= next_tick {
            engine.lock().unwrap().tick();
            next_tick = Instant::now() + interval;
        }

        loop {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    stream.set_nonblocking(false)?;
                    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                    handle_connection(engine, &stream);
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }

        if let Some(s) = sighup {
            if s.take() {
                eprintln!("fanctld: SIGHUP: reloading config");
                if let Err(e) = engine.lock().unwrap().reload() {
                    eprintln!("fanctld: config reload failed: {e}");
                }
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Answer a single request on a connected client stream.
fn handle_connection(engine: &Arc<Mutex<Engine>>, stream: &UnixStream) {
    let mut reader = std::io::BufReader::new(stream);
    let line = match ipc::read_line(&mut reader) {
        Ok(Some(l)) => l,
        _ => return, // EOF or timeout: the client is gone.
    };
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let _ = ipc::write_line(
                stream,
                &Response::Error {
                    message: format!("bad request: {e}"),
                },
            );
            return;
        }
    };
    // Lock per operation (never while reading a slow client), so the
    // engine is only held for as long as the request itself takes.
    let resp = match &req {
        Request::GetState => {
            let e = engine.lock().unwrap();
            Response::State {
                state: e.snapshot(),
            }
        }
        Request::SetMode { fan, mode } => match engine.lock().unwrap().set_mode(fan, *mode) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error { message: e },
        },
        Request::SaveFanConfig {
            fan,
            curve,
            default_mode,
        } => match engine
            .lock()
            .unwrap()
            .save_fan_config(fan, curve.clone(), *default_mode)
        {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error { message: e },
        },
        Request::Sweep { fan, step, settle } => {
            let step = step.unwrap_or(sweep::DEFAULT_STEP);
            let settle = settle.unwrap_or(sweep::DEFAULT_SETTLE_SECS);
            // Validate (and mark) the sweep under the lock, so the client
            // gets an immediate answer; the work itself runs in the
            // background and reports through the state snapshots.
            match engine.lock().unwrap().sweep_begin(fan) {
                Ok(()) => {
                    let eng = Arc::clone(engine);
                    let fan = fan.clone();
                    std::thread::spawn(move || run_sweep(&eng, &fan, step, settle));
                    Response::Ok
                }
                Err(e) => Response::Error { message: e },
            }
        }
        Request::ReloadConfig => match engine.lock().unwrap().reload() {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error { message: e },
        },
    };
    let _ = ipc::write_line(stream, &resp);
}

/// Drive one full calibration sweep in a background thread: write a duty,
/// sleep (with the engine lock *released*, so the daemon can keep ticking
/// the other fans and answering clients), read the tachometer, and repeat
/// until the RPM stops rising or 100 % is reached.
fn run_sweep(engine: &Arc<Mutex<Engine>>, fan: &str, step: u8, settle: f64) {
    let sleep = Duration::from_secs_f64(settle);
    let mut duty = 0u8;
    loop {
        // Write the next duty. A write failure (or a config reload that
        // dropped the fan) ends the sweep.
        let write = {
            let mut e = engine.lock().unwrap();
            if e.sweep.as_ref().is_none_or(|s| s.fan != fan) {
                return;
            }
            e.sweep_write(duty)
        };
        if let Err(why) = write {
            engine.lock().unwrap().sweep_abort(why);
            return;
        }
        // Let the fan reach its new speed — with the lock released.
        std::thread::sleep(sleep);
        let rpm = {
            let mut e = engine.lock().unwrap();
            if e.sweep.as_ref().is_none_or(|s| s.fan != fan) {
                return;
            }
            e.sweep_read()
        };
        let next = {
            let mut e = engine.lock().unwrap();
            e.sweep_record(duty, rpm);
            match e.sweep.as_ref() {
                Some(s) if s.fan == fan => {
                    if duty == 100 || sweep::stalled(&s.steps) {
                        None
                    } else {
                        Some(duty.saturating_add(step).min(100))
                    }
                }
                _ => return,
            }
        };
        match next {
            Some(n) => duty = n,
            None => break,
        }
    }
    engine.lock().unwrap().sweep_finish();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{InitialMode, TempSensor};
    use crate::curve::CurvePoint;
    use crate::ipc::Client;
    use std::fs;

    /// A fake hwmon tree: one k10temp at 70 °C driving one it8792 PWM
    /// (with a tachometer, so sweeps can observe it).
    fn fake_base() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("hwmon0");
        fs::create_dir_all(&t).unwrap();
        fs::write(t.join("name"), "k10temp\n").unwrap();
        fs::write(t.join("temp1_input"), "70000\n").unwrap();
        fs::write(t.join("temp1_label"), "Tctl\n").unwrap();
        let p = dir.path().join("hwmon1");
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("name"), "it8792\n").unwrap();
        fs::write(p.join("pwm1"), "0\n").unwrap();
        fs::write(p.join("pwm1_enable"), "0\n").unwrap();
        fs::write(p.join("fan1_input"), "0\n").unwrap();
        dir
    }

    fn engine(base: &std::path::Path, config_path: &std::path::Path) -> Engine {
        let config = Config {
            refresh_secs: 2.0,
            pwm: vec![Pwm {
                id: "pwm1".into(),
                hwmon: "it8792".into(),
                name: Some("Fan 1".into()),
                pwm_max: 255,
                temp_sensors: vec![TempRef {
                    hwmon: "k10temp".into(),
                    sensor: "temp1".into(),
                }],
                temp_sensor: None,
                aggregation: Aggregation::Max,
                control: ControlKind::Curve,
                curve: vec![CurvePoint::new(0.0, 0.0), CurvePoint::new(100.0, 100.0)],
                default: InitialMode::Auto,
                duty_min: None,
                duty_max: None,
            }],
            temp_sensors: vec![TempSensor {
                hwmon: "k10temp".into(),
                sensors: vec!["temp1".into()],
            }],
            show_lm_sensors: false,
        };
        Engine::new(config, base.to_path_buf(), config_path.to_path_buf())
    }

    fn pwm_file(base: &std::path::Path, name: &str) -> String {
        fs::read_to_string(base.join("hwmon1").join(name))
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn tick_applies_curve_and_reads_back() {
        let base = fake_base();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        e.tick();

        // 70 °C on the 0→0 / 100→100 curve = 70% = round(0.7·255) = 179.
        assert_eq!(pwm_file(base.path(), "pwm1"), "179");
        assert_eq!(pwm_file(base.path(), "pwm1_enable"), "1");
        let fan = &e.fans[0];
        assert!(
            (fan.duty.unwrap() - 70.0).abs() < 0.5,
            "duty: {:?}",
            fan.duty
        );
        assert!(fan.err.is_none(), "{:?}", fan.err);
    }

    #[test]
    fn set_mode_manual_writes_duty() {
        let base = fake_base();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        e.tick();
        e.set_mode("pwm1", Mode::Manual { percent: 50 }).unwrap();

        assert_eq!(pwm_file(base.path(), "pwm1"), "128");
        assert_eq!(pwm_file(base.path(), "pwm1_enable"), "1");
        assert!((e.fans[0].duty.unwrap() - 50.0).abs() < 0.5);
    }

    #[test]
    fn set_mode_off_and_full_set_enable_bits() {
        let base = fake_base();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        e.tick();
        e.set_mode("pwm1", Mode::Off).unwrap();
        // Off is a 0% manual duty: the it87 family's `pwmN_enable = 0`
        // does not stop the fan (the driver keeps it on in on/off mode
        // at 100%).
        assert_eq!(pwm_file(base.path(), "pwm1_enable"), "1");
        assert_eq!(pwm_file(base.path(), "pwm1"), "0");
        e.set_mode("pwm1", Mode::Full).unwrap();
        // `Full` is manual mode at 100% (the it87 driver has no "max"
        // enable value; writing 3/4 there is a hard -EINVAL).
        assert_eq!(pwm_file(base.path(), "pwm1_enable"), "1");
        assert_eq!(pwm_file(base.path(), "pwm1"), "255");
    }

    #[test]
    fn set_mode_unknown_fan_is_an_error() {
        let base = fake_base();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        let err = e.set_mode("pwm9", Mode::Off).unwrap_err();
        assert!(err.contains("pwm9"), "{err}");
    }

    #[test]
    fn save_fan_config_persists_curve_and_default() {
        let base = fake_base();
        let cfg_path = base.path().join("fanctl.yaml");
        let mut e = engine(base.path(), &cfg_path);
        config::yaml::save(&cfg_path, &e.config).unwrap();

        let new_curve = vec![
            CurvePoint::new(30.0, 0.0),
            CurvePoint::new(60.0, 50.0),
            CurvePoint::new(80.0, 100.0),
        ];
        e.save_fan_config("pwm1", new_curve.clone(), InitialMode::Off)
            .unwrap();

        // On disk: the file now holds the new curve and initial mode.
        let reloaded = config::yaml::load(&cfg_path).unwrap();
        assert_eq!(reloaded.pwm[0].curve, new_curve);
        assert_eq!(reloaded.pwm[0].default, InitialMode::Off);
        // In memory, and in the runtime curve the next tick will follow.
        assert_eq!(e.config.pwm[0].curve, new_curve);
        assert_eq!(e.config.pwm[0].default, InitialMode::Off);
        assert_eq!(e.fans[0].curve.points[1].duty, 50.0);
    }

    #[test]
    fn save_fan_config_rejects_bad_curves_and_keeps_the_file() {
        let base = fake_base();
        let cfg_path = base.path().join("fanctl.yaml");
        let mut e = engine(base.path(), &cfg_path);
        config::yaml::save(&cfg_path, &e.config).unwrap();
        let before = std::fs::read_to_string(&cfg_path).unwrap();

        // Unsorted temperatures.
        let bad = vec![CurvePoint::new(70.0, 0.0), CurvePoint::new(60.0, 50.0)];
        let err = e
            .save_fan_config("pwm1", bad, InitialMode::Auto)
            .unwrap_err();
        assert!(err.contains("increasing"), "{err}");
        // Out-of-range duty.
        let bad_duty = vec![CurvePoint::new(30.0, 101.0)];
        let err = e
            .save_fan_config("pwm1", bad_duty, InitialMode::Auto)
            .unwrap_err();
        assert!(err.contains("duty"), "{err}");
        // Empty curve.
        let err = e
            .save_fan_config("pwm1", Vec::new(), InitialMode::Auto)
            .unwrap_err();
        assert!(err.contains("at least one"), "{err}");

        // Nothing changed: file intact, in-memory curve untouched.
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), before);
        assert_eq!(
            e.config.pwm[0].curve,
            vec![CurvePoint::new(0.0, 0.0), CurvePoint::new(100.0, 100.0)]
        );
    }

    #[test]
    fn save_fan_config_rolls_back_when_the_write_fails() {
        let base = fake_base();
        // The config path's parent is a *file*, so the temp-file write must
        // fail; the engine has to roll its in-memory state back.
        let blocker = base.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let cfg_path = blocker.join("fanctl.yaml");
        let mut e = engine(base.path(), &cfg_path);
        let (old_curve, old_default) = (e.config.pwm[0].curve.clone(), e.config.pwm[0].default);

        let err = e
            .save_fan_config(
                "pwm1",
                vec![CurvePoint::new(10.0, 10.0), CurvePoint::new(20.0, 20.0)],
                InitialMode::Full,
            )
            .unwrap_err();
        assert!(!err.is_empty(), "the write error must be reported: {err:?}");
        assert_eq!(e.config.pwm[0].curve, old_curve);
        assert_eq!(e.config.pwm[0].default, old_default);
    }

    #[test]
    fn set_mode_reports_a_failed_write() {
        let base = fake_base();
        // Make the pwm1 register a directory so the write must fail.
        let p = base.path().join("hwmon1").join("pwm1");
        fs::remove_file(&p).unwrap();
        fs::create_dir(&p).unwrap();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        let err = e
            .set_mode("pwm1", Mode::Manual { percent: 50 })
            .unwrap_err();
        assert!(err.contains("Fan 1"), "{err}");
        assert!(e.fans[0].err.is_some());
    }

    #[test]
    fn reload_rebuilds_from_file() {
        let base = fake_base();
        let cfg_path = base.path().join("fanctl.yaml");
        let mut e = engine(base.path(), &cfg_path);
        e.tick();

        // Write a new config (a second fan) and reload.
        let mut cfg = e.config.clone();
        let mut pwm2 = cfg.pwm[0].clone();
        pwm2.id = "pwm2".into();
        pwm2.default = InitialMode::Off;
        cfg.pwm.push(pwm2);
        fs::write(&cfg_path, serde_yaml::to_string(&cfg).unwrap()).unwrap();

        e.reload().unwrap();
        assert_eq!(e.fans.len(), 2);
        assert_eq!(e.fans[1].mode, Mode::Off);
        assert_eq!(pwm_file(base.path(), "pwm1_enable"), "1");
    }

    /// A fake daemon answering on a real Unix socket, in the background.
    fn fake_daemon(socket: &std::path::Path, e: Engine) -> std::thread::JoinHandle<()> {
        let socket = socket.to_path_buf();
        let no_sighup = None::<signal::Sighup>;
        let engine = Arc::new(Mutex::new(e));
        std::thread::spawn(move || {
            let _ = run(&socket, &engine, &no_sighup);
        })
    }

    #[test]
    fn client_round_trips_over_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fanctld.sock");
        let base = fake_base();
        let e = engine(base.path(), &base.path().join("fanctl.yaml"));
        fake_daemon(&socket, e);
        std::thread::sleep(std::time::Duration::from_millis(200));

        let client = Client::new(socket.clone());
        let resp = client.request(&Request::GetState).unwrap();
        match resp {
            Response::State { state } => {
                assert_eq!(state.fans.len(), 1);
                assert_eq!(state.fans[0].label, "Fan 1");
                // The one k10temp reading from the fixture (plus any real
                // NVIDIA GPUs the dev machine happens to have).
                assert!(
                    state.temps.iter().any(|t| t.label == "Tctl"),
                    "temps: {:?}",
                    state.temps
                );
                assert!(
                    (state
                        .temps
                        .iter()
                        .find(|t| t.label == "Tctl")
                        .and_then(|t| t.celsius)
                        .unwrap()
                        - 70.0)
                        .abs()
                        < 0.01
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let _ = client
            .request(&Request::SetMode {
                fan: "pwm1".into(),
                mode: Mode::Manual { percent: 30 },
            })
            .unwrap();
        let resp = client.request(&Request::GetState).unwrap();
        match resp {
            Response::State { state } => {
                assert_eq!(
                    state.fans[0].mode,
                    Mode::Manual { percent: 30 },
                    "set-mode should be visible in the next snapshot"
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn client_saves_fan_config_over_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fanctld.sock");
        let base = fake_base();
        let cfg_path = base.path().join("fanctl.yaml");
        let e = engine(base.path(), &cfg_path);
        config::yaml::save(&cfg_path, &e.config).unwrap();
        fake_daemon(&socket, e);
        std::thread::sleep(std::time::Duration::from_millis(200));

        let client = Client::new(socket);
        let resp = client
            .request(&Request::SaveFanConfig {
                fan: "pwm1".into(),
                curve: vec![CurvePoint::new(30.0, 10.0), CurvePoint::new(75.0, 90.0)],
                default_mode: InitialMode::Off,
            })
            .unwrap();
        assert!(matches!(resp, Response::Ok), "{resp:?}");

        // The next snapshot must already carry the new curve and mode.
        let resp = client.request(&Request::GetState).unwrap();
        match resp {
            Response::State { state } => {
                assert_eq!(
                    state.fans[0].curve,
                    vec![CurvePoint::new(30.0, 10.0), CurvePoint::new(75.0, 90.0)]
                );
                assert_eq!(state.fans[0].default_mode, InitialMode::Off);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    /// Write `value` to the fake chip's tachometer file.
    fn set_tach(base: &std::path::Path, value: &str) {
        fs::write(base.join("hwmon1").join("fan1_input"), value).unwrap();
    }

    #[test]
    fn sweep_end_to_end_derives_range_and_restores_mode() {
        let base = fake_base();
        let cfg_path = base.path().join("fanctl.yaml");
        let mut e = engine(base.path(), &cfg_path);
        config::yaml::save(&cfg_path, &e.config).unwrap();
        e.tick(); // the 0→100 curve at 70 °C writes 179

        e.sweep_begin("pwm1").unwrap();
        // 0 %: the fan is off.
        e.sweep_write(0).unwrap();
        assert_eq!(pwm_file(base.path(), "pwm1"), "0");
        set_tach(base.path(), "0");
        assert_eq!(e.sweep_read(), 0);
        e.sweep_record(0, 0);
        // 10 %: it starts (950), 20 % (1400), 30 % (1800) …
        for (duty, rpm) in [(10, "950"), (20, "1400"), (30, "1800")] {
            e.sweep_write(duty).unwrap();
            set_tach(base.path(), rpm);
            assert_eq!(e.sweep_read(), rpm.parse::<u32>().unwrap());
            e.sweep_record(duty, rpm.parse::<u32>().unwrap());
        }
        // … 40 %: the RPM no longer rises (noise) → the fan is saturated.
        e.sweep_write(40).unwrap();
        set_tach(base.path(), "1805");
        assert_eq!(e.sweep_read(), 1805);
        e.sweep_record(40, 1805);
        assert!(sweep::stalled(&e.sweep.as_ref().unwrap().steps));

        e.sweep_finish();

        // The range was derived and saved to the config (file + memory),
        // and it now clamps the curve.
        let r = e.last_sweep.as_ref().unwrap();
        assert_eq!(
            (r.min_duty, r.max_duty, r.saturated),
            (Some(10), Some(30), true)
        );
        assert!(r.note.contains("between 10 % and 30 %"), "{}", r.note);
        assert!(r.note.contains("duty_min=10 / duty_max=30"), "{}", r.note);
        let reloaded = config::yaml::load(&cfg_path).unwrap();
        assert_eq!(reloaded.pwm[0].duty_min, Some(10.0));
        assert_eq!(reloaded.pwm[0].duty_max, Some(30.0));
        assert_eq!(e.fans[0].duty_min, Some(10.0));

        // The fan's pre-sweep mode (auto/curve) was restored: the next
        // tick re-applies the curve, now clamped to 10–30 % (70 °C → 70 %
        // → clamped to 30 % → round(0.3·255) = 77).
        assert_eq!(e.sweep, None);
        e.tick();
        assert_eq!(pwm_file(base.path(), "pwm1"), "77");
        assert_eq!(e.fans[0].mode, Mode::Auto);
    }

    #[test]
    fn sweep_begin_rejects_unknown_fans_concurrent_sweeps_and_missing_tachs() {
        let base = fake_base();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        let err = e.sweep_begin("pwm9").unwrap_err();
        assert!(err.contains("pwm9"), "{err}");

        // No tachometer file on the chip.
        let base = tempfile::tempdir().unwrap();
        let p = base.path().join("hwmon1");
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("name"), "it8792\n").unwrap();
        fs::write(p.join("pwm1"), "0\n").unwrap();
        fs::write(p.join("pwm1_enable"), "0\n").unwrap();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        let err = e.sweep_begin("pwm1").unwrap_err();
        assert!(err.contains("tachometer"), "{err}");

        // A second sweep is refused while one is running.
        let base = fake_base();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        e.sweep_begin("pwm1").unwrap();
        let err = e.sweep_begin("pwm1").unwrap_err();
        assert!(err.contains("already"), "{err}");
    }

    #[test]
    fn tick_keeps_the_swept_fans_duty_but_refreshes_its_readback() {
        let base = fake_base();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        e.tick(); // curve: 70 °C → 179
        e.sweep_begin("pwm1").unwrap();
        e.sweep_write(40).unwrap();
        assert_eq!(pwm_file(base.path(), "pwm1"), "102");

        // A tick must not re-apply the curve (70 °C → 179) to the swept
        // fan, but it must still refresh the displayed duty/rpm.
        set_tach(base.path(), "777");
        e.tick();
        assert_eq!(pwm_file(base.path(), "pwm1"), "102");
        assert_eq!(e.fans[0].rpm, Some(777));

        // Mode changes are refused while the sweep runs.
        let err = e
            .set_mode("pwm1", Mode::Manual { percent: 50 })
            .unwrap_err();
        assert!(err.contains("sweep"), "{err}");
    }

    #[test]
    fn the_curve_is_clamped_to_the_measured_range() {
        let base = fake_base();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        // A hand-edited (or swept-in) range: the 0→100 curve at 10 °C
        // evaluates to 10 %, which is clamped up to duty_min = 20 %.
        let (min, max) = sweep::normalize(Some(20.0), Some(80.0));
        e.fans[0].duty_min = min;
        e.fans[0].duty_max = max;
        // 70 °C → 70 % (inside the range, untouched); 10 °C → 10 % → 20 %.
        e.tick();
        assert_eq!(pwm_file(base.path(), "pwm1"), "179");
        fs::write(base.path().join("hwmon0").join("temp1_input"), "10000\n").unwrap();
        e.tick();
        assert_eq!(pwm_file(base.path(), "pwm1"), "51"); // 20 % of 255
                                                         // And a hot 130 °C → 100 % → clamped down to 80 %.
        fs::write(base.path().join("hwmon0").join("temp1_input"), "130000\n").unwrap();
        e.tick();
        assert_eq!(pwm_file(base.path(), "pwm1"), "204"); // 80 % of 255
    }

    #[test]
    fn sweep_over_socket_reports_progress_and_result_in_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fanctld.sock");
        let base = fake_base();
        let cfg_path = base.path().join("fanctl.yaml");
        let e = engine(base.path(), &cfg_path);
        config::yaml::save(&cfg_path, &e.config).unwrap();
        // The fake tachometer (static: the same reading at every duty — a
        // "stuck" one). Set it before the daemon starts, so the sweep
        // thread never sees the initial 0.
        set_tach(base.path(), "1234");
        fake_daemon(&socket, e);
        std::thread::sleep(std::time::Duration::from_millis(200));

        let client = Client::new(socket);
        // Coarse steps, 200 ms settling: the sweep runs about 600 ms, long
        // enough for the polling below to see the "sweeping" state.
        let resp = client
            .request(&Request::Sweep {
                fan: "pwm1".into(),
                step: Some(50),
                settle: Some(0.2),
            })
            .unwrap();
        assert!(matches!(resp, Response::Ok), "{resp:?}");

        // The daemon reports the running (and then finished) sweep in its
        // snapshots; poll until the result shows up.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_sweeping = false;
        let mut report = None;
        while std::time::Instant::now() < deadline {
            let resp = client.request(&Request::GetState).unwrap();
            if let Response::State { state } = resp {
                if state.sweeping.as_deref() == Some("pwm1") {
                    saw_sweeping = true;
                }
                if let Some(r) = state.sweep {
                    if r.fan == "pwm1" {
                        report = Some(r);
                        break;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            saw_sweeping,
            "the snapshot should show the fan while it sweeps"
        );
        let r = report.expect("the finished sweep should show up in a snapshot");
        // A constant 1234 rpm at every duty: the "fan" is already spinning
        // at 0 % and never changes — a degenerate (0, 0) range that must
        // *not* be persisted.
        assert_eq!(
            (r.min_duty, r.max_duty, r.saturated),
            (Some(0), Some(0), true)
        );
        assert!(r.note.contains("stuck tach"), "{}", r.note);
        let reloaded = config::yaml::load(&cfg_path).unwrap();
        assert_eq!(
            reloaded.pwm[0].duty_min, None,
            "a degenerate range is not saved"
        );
        assert_eq!(
            reloaded.pwm[0].duty_max, None,
            "a degenerate range is not saved"
        );
    }

    #[test]
    fn sweep_aborts_when_a_write_fails() {
        let base = fake_base();
        // Make the pwm1 register a directory so the write must fail.
        let p = base.path().join("hwmon1").join("pwm1");
        fs::remove_file(&p).unwrap();
        fs::create_dir(&p).unwrap();
        let mut e = engine(base.path(), &base.path().join("fanctl.yaml"));
        e.tick(); // resolves the chip (the read of the "pwm1" dir fails harmlessly)
        e.sweep_begin("pwm1").unwrap();
        let err = e.sweep_write(0).unwrap_err();
        assert!(!err.is_empty(), "{err:?}");
        e.sweep_abort(err);
        assert_eq!(e.sweep, None);
        assert!(e.status.contains("sweep of pwm1"), "{}", e.status);
        // The fan's mode was restored.
        assert_eq!(e.fans[0].mode, Mode::Auto);
    }
}
