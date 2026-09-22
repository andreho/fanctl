//! `fanctld` — the daemon. Owns the `hwmon` interface: it reads temperatures,
//! applies each fan's mode from its config, and serves `fanctlui` clients
//! over a Unix socket. Run it as a service (see `systemd/fanctld.service`);
//! send it `SIGHUP` to reload its config.
//!
//! One-shot modes (no daemon involved): `--probe` lists the discovered
//! hardware; `--summary` prints a one-shot text dump; `--set` applies
//! fan modes directly to the chips and exits; `--sweep` runs a
//! calibration of one fan (its duty range) and exits.
//!
//! Note for the one-shot `--set`/`--sweep` forms: they write the chips
//! directly. If a `fanctld` service is already running, its next tick
//! re-applies its own state to the fans — use `fanctlui --set` instead
//! to route a one-shot through the running daemon (where a manual duty
//! sticks), or calibrate from the TUI (key `c`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use fanctl::{config, ctl, daemon, ipc, summary, sweep};

struct Args {
    sock: Option<PathBuf>,
    config_path: Option<PathBuf>,
    probe: bool,
    summary: bool,
    sets: Vec<String>,
    sweep: Option<String>,
    sweep_step: Option<u8>,
    sweep_settle: Option<f64>,
}

fn print_usage() {
    println!(
        "fanctld — the fanctl fan-control daemon

Usage: fanctld [OPTIONS]

Options:
  -c, --config FILE   Use FILE as the config file
                      (default: $XDG_CONFIG_HOME/fanctl/fanctl.yaml or
                      ~/.config/fanctl/fanctl.yaml; generated on first run)
      --sock PATH     Listen on the Unix socket PATH
                      (default: $XDG_RUNTIME_DIR/fanctld.sock or /run/fanctld.sock;
                      can also be set via the FANCTLD_SOCK environment variable)
      --probe         List the discovered hwmon chips (and NVIDIA GPUs) and exit
      --summary       Print a one-shot text dump of the PWM channels, fans,
                      and temperatures, then exit
      --set FAN=MODE  One-shot: apply MODE to the fan FAN (its pwmN id) and
                      exit. MODE is a percent (0-100) or one of
                      auto|off|full, e.g. --set pwm1=99 --set pwm2=off. May
                      be repeated. Writes the chips directly: a running
                      daemon will re-apply its own state on its next tick,
                      so route one-shots through it with `fanctlui --set`
                      instead.
      --sweep FAN     One-shot calibration of the fan FAN (its pwmN id):
                      sweep its duty from 0 % to 100 % (in --step percent
                      increments, waiting --settle seconds for it to
                      settle after each change), read its tachometer, and
                      derive the range of duty in which the fan actually
                      responds (it does not spin below the start, and it
                      saturates at the top). A useful range is saved as
                      duty_min/duty_max on the fan in the config — its
                      curve is then clamped to that range — and the fan's
                      previous mode is restored afterwards. The daemon
                      must NOT be running (its ticks would stomp the
                      sweep); calibrate from the TUI (key c) instead if
                      it is.
      --step N        The sweep's duty increment, in percent (default 10,
                      max 50); requires --sweep
      --settle S      Seconds to wait for the fan to settle after each
                      duty change, before reading its rpm (default 3,
                      min 0.2, max 60); requires --sweep
  -h, --help          Show this help
  -V, --version       Show the version

Run in the background (e.g. as a systemd service) to keep your fans under
control; connect to it with the fanctlui TUI."
    );
}

fn fail(msg: &str) -> ! {
    eprintln!("fanctld: {msg}");
    eprintln!("Run 'fanctld --help' for usage.");
    std::process::exit(2);
}

fn value<'a>(args: &'a [String], i: &mut usize, flag: &str) -> &'a String {
    *i += 1;
    args.get(*i).unwrap_or_else(|| {
        eprintln!("fanctld: missing value for {flag}");
        std::process::exit(2);
    })
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut sock: Option<PathBuf> = None;
    let mut config_path: Option<PathBuf> = None;
    let mut probe = false;
    let mut summary_mode = false;
    let mut sets: Vec<String> = Vec::new();
    let mut sweep: Option<String> = None;
    let mut sweep_step: Option<u8> = None;
    let mut sweep_settle: Option<f64> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--config" => {
                let v = value(&args, &mut i, "-c");
                config_path = Some(PathBuf::from(v));
            }
            "--sock" => {
                let v = value(&args, &mut i, "--sock");
                sock = Some(PathBuf::from(v));
            }
            "--probe" => probe = true,
            "--summary" => summary_mode = true,
            "--set" => {
                let v = value(&args, &mut i, "--set");
                sets.push(v.clone());
            }
            "--sweep" => {
                let v = value(&args, &mut i, "--sweep");
                sweep = Some(v.clone());
            }
            "--step" => {
                let v = value(&args, &mut i, "--step");
                match v.trim().parse::<u8>() {
                    Ok(n) if (1..=50).contains(&n) => sweep_step = Some(n),
                    _ => fail(&format!(
                        "bad --step value {v:?}: expected a percent in 1..=50"
                    )),
                }
            }
            "--settle" => {
                let v = value(&args, &mut i, "--settle");
                match v.trim().parse::<f64>() {
                    Ok(s) if (0.2..=60.0).contains(&s) => sweep_settle = Some(s),
                    _ => fail(&format!(
                        "bad --settle value {v:?}: expected seconds in 0.2..=60"
                    )),
                }
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("fanctld {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => fail(&format!("unknown flag {other:?}")),
        }
        i += 1;
    }
    if (sweep_step.is_some() || sweep_settle.is_some()) && sweep.is_none() {
        fail("--step and --settle require --sweep");
    }
    Args {
        sock,
        config_path,
        probe,
        summary: summary_mode,
        sets,
        sweep,
        sweep_step,
        sweep_settle,
    }
}

/// One-shot `--sweep FAN`: runs a calibration sweep directly against the
/// chips (there is no daemon loop in this process), derives the fan's
/// effective duty range, saves it to the config when it is useful, and
/// restores the fan's previous mode. Returns the exit code: 0 when a
/// usable range was found, 1 otherwise.
fn one_shot_sweep(
    fan: &str,
    step: u8,
    settle: f64,
    sock: &Path,
    cfg: &config::Config,
    config_path: &Path,
) -> i32 {
    // Refuse to run while a daemon owns the fans: its next tick would
    // stomp the sweep's duty.
    let client = ipc::Client::new(sock.to_path_buf());
    if let Ok(ipc::Response::State { .. }) = client.request(&ipc::Request::GetState) {
        eprintln!(
            "fanctld: a fanctld daemon is running (socket {}): its ticks would stomp the sweep.",
            sock.display()
        );
        eprintln!(
            "fanctld: stop it first, or calibrate from the TUI instead (select the fan, press c)."
        );
        std::process::exit(2);
    }

    let base = Path::new(fanctl::hwmon::SYS_HWMON).to_path_buf();
    let mut engine = daemon::Engine::new(cfg.clone(), base, config_path.to_path_buf());
    // Resolve the chips and take an initial read-back (the sweep needs
    // both the chip and a tachometer to be present).
    engine.tick();

    if let Err(e) = engine.sweep_begin(fan) {
        eprintln!("fanctld: {e}");
        return 1;
    }

    println!("Sweeping {fan}: {step} % per step, {settle} s settling after each change.");
    println!("The fan's previous mode is restored when it finishes.\n");
    println!("   duty      rpm");

    let sleep = Duration::from_secs_f64(settle);
    let mut duty = 0u8;
    let mut failed = false;
    loop {
        if let Err(e) = engine.sweep_write(duty) {
            eprintln!("\nfanctld: {e}");
            failed = true;
            break;
        }
        std::thread::sleep(sleep);
        let rpm = engine.sweep_read();
        engine.sweep_record(duty, rpm);
        println!("  {duty:3} %   {rpm:>6}");
        if duty == 100 || sweep::stalled(&engine.sweep.as_ref().unwrap().steps) {
            break;
        }
        duty = duty.saturating_add(step).min(100);
    }

    engine.sweep_finish();
    let report = engine.last_sweep.clone();
    if let Some(r) = &report {
        println!();
        println!("{}", r.note);
    }
    if failed {
        return 1;
    }
    match report {
        Some(r) if r.min_duty.is_some() && r.max_duty.is_some() && r.min_duty != r.max_duty => 0,
        // The fan never spun, or only at a single duty: no usable range.
        _ => 1,
    }
}

fn main() {
    let args = parse_args();

    // The one-shot modes are mutually exclusive.
    let one_shot_count = [
        args.probe,
        args.summary,
        !args.sets.is_empty(),
        args.sweep.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if one_shot_count > 1 {
        fail("--probe, --summary, --set and --sweep are one-shot modes and cannot be combined");
    }

    if args.probe {
        summary::probe_view();
        return;
    }

    let config_path = args.config_path.unwrap_or_else(config::default_path);
    let cfg = match config::load_or_generate(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fanctld: {e}");
            std::process::exit(1);
        }
    };

    if args.summary {
        summary::summary_view(&cfg, std::path::Path::new(fanctl::hwmon::SYS_HWMON));
        return;
    }

    // The socket the daemon would serve on: --sock, or $FANCTLD_SOCK, or
    // the default.
    let sock = args
        .sock
        .or_else(|| std::env::var("FANCTLD_SOCK").ok().map(PathBuf::from))
        .unwrap_or_else(ipc::default_socket_path);

    if let Some(fan) = &args.sweep {
        let step = args.sweep_step.unwrap_or(fanctl::sweep::DEFAULT_STEP);
        let settle = args
            .sweep_settle
            .unwrap_or(fanctl::sweep::DEFAULT_SETTLE_SECS);
        std::process::exit(one_shot_sweep(fan, step, settle, &sock, &cfg, &config_path));
    }

    if !args.sets.is_empty() {
        // Parse every pair up front, so a bad one fails before any write.
        let parsed: Vec<ctl::Set> = args
            .sets
            .iter()
            .map(|s| match ctl::parse_set(s) {
                Ok(p) => p,
                Err(e) => fail(&e),
            })
            .collect();

        let base = std::path::Path::new(fanctl::hwmon::SYS_HWMON).to_path_buf();
        let mut engine = daemon::Engine::new(cfg, base, config_path);
        let mut failed = false;
        for p in &parsed {
            match engine.set_mode(&p.fan, p.mode) {
                Ok(()) => println!("{}: set to {}", p.fan, p.mode.label()),
                Err(e) => {
                    eprintln!("fanctld: {}: {e}", p.fan);
                    failed = true;
                }
            }
        }
        std::process::exit(failed as i32);
    }

    let engine = daemon::Engine::new(
        cfg,
        std::path::Path::new(fanctl::hwmon::SYS_HWMON).to_path_buf(),
        config_path,
    );

    let sighup = daemon::signal::Sighup::new().ok();
    // The engine sits behind a `Mutex` (shared with the calibration-sweep
    // threads, which borrow it only briefly).
    let engine = std::sync::Arc::new(std::sync::Mutex::new(engine));
    if let Err(e) = daemon::run(&sock, &engine, &sighup) {
        eprintln!("fanctld: {e}");
        std::process::exit(1);
    }
}
