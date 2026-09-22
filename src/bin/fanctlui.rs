//! `fanctlui` — the client. Connects to the `fanctld` daemon over its Unix
//! socket, renders its state (fans, temperatures), and sends fan commands.
//! It never talks to the hardware directly.
//!
//! It has two faces: the interactive TUI (the default), and the one-shot
//! `--set FAN=MODE` command, which applies a mode through the daemon and
//! exits — handy for scripts (`--set pwm1=99 --set pwm2=off`).

use std::path::PathBuf;

use fanctl::{ctl, ipc, tui};

struct Args {
    sock: Option<PathBuf>,
    sets: Vec<String>,
}

fn print_usage() {
    println!(
        "fanctlui — the fanctl TUI client

Usage: fanctlui [OPTIONS]

Options:
  -s, --sock PATH   Connect to the daemon's Unix socket PATH
                    (default: $XDG_RUNTIME_DIR/fanctld.sock or /run/fanctld.sock;
                    can also be set via the FANCTLD_SOCK environment variable)
      --set FAN=MODE  One-shot: apply MODE to the fan FAN (its pwmN id)
                      through the daemon and exit. MODE is a percent (0-100)
                      or one of auto|off|full, e.g. --set pwm1=99. May be
                      repeated; the TUI is not started.
  -h, --help        Show this help
  -V, --version     Show the version

Keys in the TUI: a/f/o = auto/full/off, Enter = exact duty (0-100),
  e = edit curve and default mode (s saves),
  c = calibrate the fan's duty range (sweep, saved as duty_min/duty_max),
  s = sort, ? = help, q or Ctrl+C = quit."
    );
}

fn fail(msg: &str) -> ! {
    eprintln!("fanctlui: {msg}");
    eprintln!("Run 'fanctlui --help' for usage.");
    std::process::exit(2);
}

fn value<'a>(args: &'a [String], i: &mut usize, flag: &str) -> &'a String {
    *i += 1;
    args.get(*i).unwrap_or_else(|| {
        eprintln!("fanctlui: missing value for {flag}");
        std::process::exit(2);
    })
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut sock: Option<PathBuf> = None;
    let mut sets: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-s" | "--sock" => {
                let v = value(&args, &mut i, "-s");
                sock = Some(PathBuf::from(v));
            }
            "--set" => {
                let v = value(&args, &mut i, "--set");
                sets.push(v.clone());
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("fanctlui {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => fail(&format!("unknown flag {other:?}")),
        }
        i += 1;
    }
    Args { sock, sets }
}

fn main() {
    let args = parse_args();
    let sock = args
        .sock
        .or_else(|| std::env::var("FANCTLD_SOCK").ok().map(PathBuf::from))
        .unwrap_or_else(ipc::default_socket_path);

    if !args.sets.is_empty() {
        // One-shot mode: no terminal involved, so no TTY guard either.
        let parsed: Vec<ctl::Set> = args
            .sets
            .iter()
            .map(|s| match ctl::parse_set(s) {
                Ok(p) => p,
                Err(e) => fail(&e),
            })
            .collect();

        let client = ipc::Client::new(sock);
        let mut failed = false;
        for p in &parsed {
            match client.request(&ipc::Request::SetMode {
                fan: p.fan.clone(),
                mode: p.mode,
            }) {
                Ok(ipc::Response::Ok) => {
                    println!("{}: set to {} (via fanctld)", p.fan, p.mode.label())
                }
                Ok(ipc::Response::Error { message }) => {
                    eprintln!("fanctlui: {}: {message}", p.fan);
                    failed = true;
                }
                Ok(other) => {
                    eprintln!("fanctlui: {}: unexpected response {other:?}", p.fan);
                    failed = true;
                }
                Err(e) => {
                    let msg = format!("{e}");
                    // The usual cause: no daemon behind the socket.
                    let hint = if msg.contains("Connection refused") || msg.contains("No such file")
                    {
                        " (is fanctld running?)"
                    } else {
                        ""
                    };
                    eprintln!("fanctlui: {}: {e}{hint}", p.fan);
                    failed = true;
                }
            }
        }
        std::process::exit(failed as i32);
    }

    // The TUI needs a real terminal on both ends. Without one (piped
    // output, a dead pty, …) raw mode is a no-op and the app would just
    // freeze with a blank screen instead of showing anything.
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("fanctlui: stdin and stdout must be a terminal (got a pipe or dead pty).");
        eprintln!("fanctlui: use 'fanctld --summary' for a one-shot text dump instead.");
        std::process::exit(1);
    }

    let client = ipc::Client::new(sock);
    let mut app = tui::App::new(client);

    let mut terminal = ratatui::init();
    // Catch panics so the terminal is restored even on an internal error —
    // a killed or panicking TUI otherwise leaves the screen in raw mode /
    // alternate screen and looks permanently frozen (`reset` undoes that).
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| app.run(&mut terminal)));
    ratatui::restore();

    match res {
        Ok(Err(e)) => {
            eprintln!("fanctlui: {e}");
            std::process::exit(1);
        }
        Ok(Ok(())) => {}
        Err(_) => {
            eprintln!("fanctlui: the TUI panicked; the terminal has been restored.");
            std::process::exit(1);
        }
    }
}
