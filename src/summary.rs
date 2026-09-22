//! One-shot, non-interactive views of the hardware: the `--probe` listing
//! and the `--summary` dump, both served by the `fanctld` binary.

use std::path::Path;

use crate::config::types::Config;
use crate::hwmon;
use crate::temps;

/// The current state of one `pwmN` channel, as `hwmon` sees it.
#[derive(Debug)]
pub struct PwmChannel {
    /// The chip the channel belongs to (its `hwmonN` directory).
    pub chip_dir: String,
    /// The chip's `name` file (e.g. `it8792`).
    pub chip_name: String,
    /// The channel number (from `pwm3`).
    pub number: u32,
    /// The channel's id string (`pwm3`).
    pub id: String,
    /// The raw duty register value, if readable.
    pub raw: Option<u32>,
    /// The duty as a percentage of the chip's range, if computable.
    pub percent: Option<f64>,
    /// The `pwmN_enable` mode bits, if readable.
    pub enable: Option<u32>,
}

/// List every PWM channel of every discovered `hwmon` chip, with the chip
/// each one belongs to and its current duty/mode. This is the "which
/// channel do I address?" view: channels that are *not* part of the config
/// show up here too.
pub fn pwm_channels_view(base: &Path) -> Vec<PwmChannel> {
    let mut out = Vec::new();
    for chip in hwmon::discover(base) {
        for n in chip.pwm_numbers() {
            out.push(PwmChannel {
                chip_dir: chip.dir.clone(),
                chip_name: chip.name.clone(),
                number: n,
                id: format!("pwm{n}"),
                raw: chip.pwm_raw(n),
                percent: chip.pwm_percent(n, 255),
                enable: chip.pwm_enable(n),
            });
        }
    }
    out
}

/// A human-readable one-line rendering of a [`PwmChannel`].
pub fn format_pwm_channel(c: &PwmChannel) -> String {
    let raw = c.raw.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into());
    let pct = c
        .percent
        .map(|p| format!("{p:4.0}%"))
        .unwrap_or_else(|| "  n/a".into());
    let mode = match c.enable {
        Some(0) => "off".to_string(),
        Some(1) => "manual".to_string(),
        Some(2) => "auto (chip)".to_string(),
        Some(_) => "driver-specific".to_string(),
        None => "n/a".to_string(),
    };
    format!(
        "  {chip_dir:8}  {chip_name:10}  {id:8}  raw={raw:3}  duty={pct}  mode={mode}",
        chip_dir = c.chip_dir,
        chip_name = c.chip_name,
        id = c.id
    )
}

/// Print every discovered hwmon chip (with its PWM/temp/fan channels) and the
/// NVIDIA GPUs NVML reports.
pub fn probe_view() {
    for c in hwmon::discover(Path::new(hwmon::SYS_HWMON)) {
        println!(
            "{:10}  {}  pwm={:?} temp={:?} fan={:?}",
            c.name,
            c.dir,
            c.pwm_numbers(),
            c.temp_numbers(),
            c.fan_numbers()
        );
    }
    let gpus = temps::read_nvidia();
    if !gpus.is_empty() {
        println!();
        println!("NVIDIA GPUs (NVML):");
        for g in &gpus {
            let temp = g
                .temp_c
                .map(|t| format!("{t:.0}"))
                .unwrap_or_else(|| "n/a".into());
            println!("  gpu{}  {:24}  {temp:>5}°C", g.index, g.name);
        }
    }
}

/// Print the one-shot summary to stdout.
pub fn summary_view(cfg: &Config, base: &Path) {
    let mut out = std::io::stdout();
    render_summary(&mut out, cfg, base).ok();
}

/// Render the one-shot summary (PWM channels, configured fans, temperatures,
/// GPUs) to `w`.
fn render_summary<W: std::io::Write>(w: &mut W, cfg: &Config, base: &Path) -> std::io::Result<()> {
    let chips = hwmon::discover(base);

    // First: every PWM channel the hardware exposes (whether or not it is
    // in the config), so the user knows which channel to address.
    let channels = pwm_channels_view(base);
    writeln!(w, "PWM channels")?;
    if channels.is_empty() {
        writeln!(w, "  (no pwmN channels found)")?;
    }
    for c in &channels {
        writeln!(w, "{}", format_pwm_channel(c))?;
    }

    writeln!(w, "\nFans")?;
    for p in &cfg.pwm {
        let Some(chip) = hwmon::resolve(&chips, &p.hwmon) else {
            writeln!(
                w,
                "  {id:8}  (chip {hwmon} not found)",
                id = p.id,
                hwmon = p.hwmon
            )?;
            continue;
        };
        let Some(n) = p.index() else {
            writeln!(w, "  {id:8}  (unparseable id)", id = p.id)?;
            continue;
        };
        let pct = chip
            .pwm_percent(n, p.pwm_max)
            .map(|v| format!("{v:4.0}%"))
            .unwrap_or_else(|| "  n/a".into());
        let mode = match chip.pwm_enable(n) {
            Some(0) => "off",
            Some(1) => "manual",
            Some(2) => "auto (chip)",
            Some(_) => "driver-specific",
            None => "n/a",
        };
        // A measured duty range (calibration sweep) is shown when present.
        let range = match (p.duty_min, p.duty_max) {
            (Some(lo), Some(hi)) => format!("  range={lo:.0}-{hi:.0}% (measured)"),
            _ => String::new(),
        };
        writeln!(
            w,
            "  {:8}  {:8}  {n:3}  duty={pct}  mode={mode}{range}",
            p.display_name(),
            p.hwmon
        )?;
        let sensors = p.driving_sensors();
        let agg = match p.aggregation {
            crate::config::types::Aggregation::Max => "max",
            crate::config::types::Aggregation::Avg => "avg",
            crate::config::types::Aggregation::Min => "min",
        };
        let refs: Vec<String> = sensors
            .iter()
            .map(|s| format!("{}:{}", s.hwmon, s.sensor))
            .collect();
        if !refs.is_empty() {
            writeln!(w, "  └─ driven by {agg}({})", refs.join(", "))?;
        }
    }

    writeln!(w, "\nTemperatures")?;
    for ts in &cfg.temp_sensors {
        let Some(chip) = hwmon::resolve(&chips, &ts.hwmon) else {
            continue;
        };
        for s in &ts.sensors {
            match crate::temps::read_hwmon(chip, s) {
                Some(r) => writeln!(
                    w,
                    "  {hwmon:12} {s:6}  {temp:6.1}°C  ({label})",
                    hwmon = ts.hwmon,
                    temp = r.celsius,
                    label = r.label
                )?,
                None => writeln!(w, "  {hwmon:12} {s:6}  -- no reading --", hwmon = ts.hwmon)?,
            }
        }
    }

    let gpus = temps::read_nvidia();
    if !gpus.is_empty() {
        writeln!(w, "\nGPUs (NVML)")?;
        for g in &gpus {
            let temp = g
                .temp_c
                .map(|t| format!("{t:6.1}"))
                .unwrap_or_else(|| "   n/a".into());
            writeln!(
                w,
                "  gpu{index:2}  {name:24}  {temp}°C",
                index = g.index,
                name = g.name
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Aggregation, ControlKind, InitialMode, Pwm, TempRef, TempSensor};

    /// A fake chip tree with two PWM channels, one with an explicit max.
    fn fake_chip() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let chip = dir.path().join("hwmon0");
        std::fs::create_dir_all(&chip).unwrap();
        std::fs::write(chip.join("name"), "it8792\n").unwrap();
        std::fs::write(chip.join("pwm1"), "182\n").unwrap();
        std::fs::write(chip.join("pwm1_enable"), "1\n").unwrap();
        std::fs::write(chip.join("pwm1_max"), "255\n").unwrap();
        std::fs::write(chip.join("pwm2"), "100\n").unwrap();
        std::fs::write(chip.join("pwm2_enable"), "0\n").unwrap();
        dir
    }

    /// An `io::Write` that appends to a `String`, so tests can capture output.
    struct S(String);
    impl std::io::Write for S {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.push_str(std::str::from_utf8(b).unwrap());
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn pwm_channels_view_lists_every_channel_with_state() {
        let dir = fake_chip();
        let out = pwm_channels_view(dir.path());

        assert_eq!(out.len(), 2, "both pwmN channels must be listed");
        assert_eq!(out[0].id, "pwm1");
        assert_eq!(out[0].chip_dir, "hwmon0");
        assert_eq!(out[0].chip_name, "it8792");
        assert_eq!(out[0].raw, Some(182));
        assert_eq!(out[0].enable, Some(1));
        assert_eq!(out[1].id, "pwm2");
        assert_eq!(out[1].enable, Some(0));

        let line1 = format_pwm_channel(&out[0]);
        // 182/255 = 71%, mode 1 = manual.
        assert!(line1.contains("hwmon0"), "{line1}");
        assert!(line1.contains("it8792"), "{line1}");
        assert!(line1.contains("pwm1"), "{line1}");
        assert!(line1.contains("raw=182"), "{line1}");
        assert!(line1.contains("duty=  71%"), "{line1}");
        assert!(line1.contains("mode=manual"), "{line1}");

        let line2 = format_pwm_channel(&out[1]);
        // pwm2 has no pwm2_max file: the default range (0..=255) applies.
        assert!(line2.contains("pwm2"), "{line2}");
        assert!(line2.contains("duty=  39%"), "{line2}");
        assert!(line2.contains("mode=off"), "{line2}");
    }

    #[test]
    fn summary_starts_with_pwm_channels() {
        let dir = fake_chip();
        let cfg = Config {
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
                curve: vec![],
                default: InitialMode::Auto,
                duty_min: None,
                duty_max: None,
            }],
            temp_sensors: vec![TempSensor {
                hwmon: "it8792".into(),
                sensors: vec!["temp1".into()],
            }],
            show_lm_sensors: false,
        };
        std::fs::write(dir.path().join("hwmon0").join("temp1_input"), "70000\n").unwrap();

        let mut out = S(String::new());
        render_summary(&mut out, &cfg, dir.path()).unwrap();
        let out = out.0;

        assert!(out.starts_with("PWM channels"), "{out}");
        assert!(out.contains("\nFans"), "{out}");
        assert!(out.contains("pwm1"), "{out}");
        assert!(
            out.contains("pwm2"),
            "the unconfigured pwm2 must show up too: {out}"
        );
    }
}
