//! `fanctl` — a terminal-based Linux tool for fan control and temperature
//! monitoring on any hwmon-equipped machine, driven by a per-fan YAML config.
//!
//! Default invocation runs the interactive TUI. `--probe` lists the discovered
//! hwmon chips; `--summary` prints a one-shot text dump instead of the TUI.

mod config;
mod curve;
mod error;
mod hwmon;
mod temps;
mod tui;

use std::path::{Path, PathBuf};

use config::generate::generate;
use config::types::Config;
use config::yaml;
use hwmon::Hwmon;

fn main() -> std::io::Result<()> {
    let (config_arg, probe, summary) = parse_args();

    if probe {
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
        return Ok(());
    }

    let path = resolve_config_path(&config_arg);
    let cfg = load_or_generate(&path);
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fanctl: {e}");
            std::process::exit(1);
        }
    };

    if summary {
        let chips = hwmon::discover(Path::new(hwmon::SYS_HWMON));
        summary_view(&cfg, &chips);
        return Ok(());
    }

    let app = tui::App::new(cfg);
    let mut terminal = ratatui::init();
    let res = app.run(&mut terminal);
    ratatui::restore();
    res
}

/// Load the config, or generate and save a default one on first run.
fn load_or_generate(path: &Path) -> Result<Config, String> {
    if !path.exists() {
        let cfg = generate(Path::new(hwmon::SYS_HWMON));
        yaml::save(path, &cfg).map_err(|e| e.to_string())?;
        eprintln!("fanctl: generated config at {}", path.display());
        eprintln!("fanctl: edit the curves, then re-run.\n");
    }
    yaml::load(path).map_err(|e| e.to_string())
}

fn resolve_config_path(arg: &Option<PathBuf>) -> PathBuf {
    match arg {
        Some(p) => p.clone(),
        None => config::default_path(),
    }
}

/// One-shot text summary of the current fans and temperatures.
fn summary_view(cfg: &Config, chips: &[Hwmon]) {
    println!("Fans");
    for p in &cfg.pwm {
        let Some(chip) = hwmon::resolve(chips, &p.hwmon) else {
            println!("  {id:8}  (chip {hwmon} not found)", id = p.id, hwmon = p.hwmon);
            continue;
        };
        let Some(n) = p.index() else {
            println!("  {id:8}  (unparseable id)", id = p.id);
            continue;
        };
        let pct = chip
            .pwm_percent(n, p.pwm_max)
            .map(|v| format!("{v:4.0}%"))
            .unwrap_or_else(|| "  n/a".into());
        let mode = match chip.pwm_enable(n) {
            Some(0) => "off",
            Some(1) => "manual",
            Some(3) => "auto",
            Some(4) => "max",
            _ => "other",
        };
        println!(
            "  {:8}  {:8}  {n:3}  duty={pct}  mode={mode}",
            p.display_name(),
            p.hwmon
        );
        let sensors = p.driving_sensors();
        let agg = match p.aggregation {
            config::types::Aggregation::Max => "max",
            config::types::Aggregation::Avg => "avg",
            config::types::Aggregation::Min => "min",
        };
        let refs: Vec<String> = sensors
            .iter()
            .map(|s| format!("{}:{}", s.hwmon, s.sensor))
            .collect();
        if !refs.is_empty() {
            println!("  └─ driven by {agg}({})", refs.join(", "));
        }
    }

    println!("\nTemperatures");
    for ts in &cfg.temp_sensors {
        let Some(chip) = hwmon::resolve(chips, &ts.hwmon) else {
            continue;
        };
        for s in &ts.sensors {
            match temps::read_hwmon(chip, s) {
                Some(r) => println!(
                    "  {hwmon:12} {s:6}  {temp:6.1}°C  ({label})",
                    hwmon = ts.hwmon,
                    temp = r.celsius,
                    label = r.label
                ),
                None => println!("  {hwmon:12} {s:6}  -- no reading --", hwmon = ts.hwmon),
            }
        }
    }

    let gpus = temps::read_nvidia();
    if !gpus.is_empty() {
        println!("\nGPUs (NVML)");
        for g in &gpus {
            let temp = g
                .temp_c
                .map(|t| format!("{t:6.1}"))
                .unwrap_or_else(|| "   n/a".into());
            println!("  gpu{index:2}  {name:24}  {temp}°C", index = g.index, name = g.name);
        }
    }
}

fn parse_args() -> (Option<PathBuf>, bool, bool) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config = None;
    let mut probe = false;
    let mut summary = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--config" => {
                i += 1;
                if i < args.len() {
                    config = Some(PathBuf::from(&args[i]));
                }
            }
            "--probe" => probe = true,
            "--summary" => summary = true,
            _ => {}
        }
        i += 1;
    }
    (config, probe, summary)
}
