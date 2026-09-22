//! The protocol shared by the `fanctld` daemon and the `fanctlui` client:
//! newline-delimited JSON over a Unix domain socket.
//!
//! Every exchange is a short-lived, stateless round trip: the client connects,
//! writes one JSON line (a [`Request`]), reads one JSON line back (a
//! [`Response`]), and disconnects. Clients may therefore come and go at any
//! time without disturbing the daemon or each other.

use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::types::InitialMode;
use crate::curve::CurvePoint;
use crate::error::Error;

/// How long a client waits for a response from the daemon.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// The default socket path the daemon listens on (and clients connect to):
/// `$XDG_RUNTIME_DIR/fanctld.sock`, or `/run/fanctld.sock` when
/// `XDG_RUNTIME_DIR` is not set (as it isn't for a root system service).
///
/// Both the daemon (`--sock`) and the client (`--sock`, or the `FANCTLD_SOCK`
/// environment variable) can override it.
pub fn default_socket_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            return Path::new(xdg.as_str()).join("fanctld.sock");
        }
    }
    PathBuf::from("/run/fanctld.sock")
}

/// A request the client sends to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// Ask for the daemon's current state (fans, temperatures, last error).
    GetState,
    /// Change the mode of the fan identified by `fan` (its `pwmN` id).
    SetMode { fan: String, mode: Mode },
    /// Persist the fan's duty curve and initial mode to the daemon's
    /// config file (atomically) and apply them to the running engine.
    SaveFanConfig {
        fan: String,
        curve: Vec<CurvePoint>,
        default_mode: InitialMode,
    },
    /// Calibrate the fan identified by `fan` (its `pwmN` id): sweep its
    /// duty from 0 % to 100 % (in `step` percent increments, default
    /// [`crate::sweep::DEFAULT_STEP`], waiting `settle` seconds, default
    /// [`crate::sweep::DEFAULT_SETTLE_SECS`], after each change) and derive
    /// the range of duty in which the fan actually responds. The daemon
    /// runs the sweep in the background — this response is just "started"
    /// (or an immediate error) — and reports progress and the result in
    /// its [`State`] snapshot (`sweeping` / `sweep`).
    Sweep {
        fan: String,
        step: Option<u8>,
        settle: Option<f64>,
    },
    /// Re-read the config file (modes reset to the config's defaults).
    ReloadConfig,
}

/// A response the daemon sends back to a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Response {
    /// The current state, in response to [`Request::GetState`].
    State { state: State },
    /// The command succeeded, in response to [`Request::SetMode`] /
    /// [`Request::ReloadConfig`].
    Ok,
    /// The command failed; `message` explains why.
    Error { message: String },
}

/// The mode a fan is (or is requested to be) driven in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Mode {
    /// Follow the fan's configured strategy (curve or kernel auto).
    Auto,
    /// The PWM is switched off.
    Off,
    /// The PWM runs at full speed.
    Full,
    /// A fixed manual duty, in percent (0..=100).
    Manual { percent: u8 },
}

impl Mode {
    /// The mode's display label (`auto`, `off`, `full`, `50%`).
    pub fn label(self) -> String {
        match self {
            Mode::Auto => "auto".into(),
            Mode::Off => "off".into(),
            Mode::Full => "full".into(),
            Mode::Manual { percent } => format!("{percent}%"),
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

/// One temperature reading for display (hwmon, NVIDIA GPU, or lm-sensors).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Temp {
    /// Stable id: `chipDir:sensor` (hwmon), `nvidia:gpu<N>`, or
    /// `lm:chip:sensor`.
    pub refname: String,
    /// A human label (the sensor's `_label`, the GPU name, …).
    pub label: String,
    /// `"hwmon"`, `"nvidia"`, or `"lm"`.
    pub source: String,
    /// The current reading in °C, if readable.
    pub celsius: Option<f64>,
}

/// The state of one fan, as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanState {
    /// The fan's id (its `pwmN` channel).
    pub id: String,
    /// The fan's display label.
    pub label: String,
    /// The mode the fan is currently driven in.
    pub mode: Mode,
    /// The last duty percentage (read back from the chip, or the computed
    /// curve duty).
    pub duty: Option<f64>,
    /// The last tachometer reading, if the chip exposes one.
    pub rpm: Option<u32>,
    /// The last error applying the mode, if any.
    pub err: Option<String>,
    /// The fan's duty curve, in config order (for the editor dialog).
    pub curve: Vec<CurvePoint>,
    /// The initial mode configured for the fan (for the editor dialog).
    pub default_mode: InitialMode,
}

/// The daemon's full state snapshot, served in response to
/// [`Request::GetState`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// How often the daemon re-evaluates its fans, in seconds. Clients
    /// should poll at (about) this rate.
    pub refresh_secs: f64,
    /// The daemon's last error, or empty if all is well.
    pub status: String,
    /// The fans, in config order.
    pub fans: Vec<FanState>,
    /// The temperature readings to display.
    pub temps: Vec<Temp>,
    /// The fan currently being calibrated (a sweep in progress), if any.
    pub sweeping: Option<String>,
    /// The last finished calibration sweep, if any (clients show its
    /// `note` and, with it, the fan's measured duty range).
    pub sweep: Option<crate::sweep::SweepReport>,
}

/// A client that sends requests to the daemon over its Unix socket.
#[derive(Debug, Clone)]
pub struct Client {
    pub socket_path: PathBuf,
}

impl Client {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Open a short-lived connection to the daemon.
    pub fn connect(&self) -> Result<UnixStream, Error> {
        let stream = UnixStream::connect(&self.socket_path)?;
        stream
            .set_read_timeout(Some(RESPONSE_TIMEOUT))
            .map_err(|e| {
                Error::Msg(format!(
                    "setting read timeout on {}: {e}",
                    self.socket_path.display()
                ))
            })?;
        Ok(stream)
    }

    /// Send `req` and read the daemon's response.
    pub fn request(&self, req: &Request) -> Result<Response, Error> {
        let mut stream = self.connect()?;
        let line = serde_json::to_string(req)?;
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;

        let mut reply = String::new();
        let n = std::io::BufReader::new(&stream).read_line(&mut reply)?;
        if n == 0 {
            return Err(Error::Msg("fanctld closed the connection".into()));
        }
        serde_json::from_str(&reply).map_err(|e| Error::Msg(format!("fanctld: bad response: {e}")))
    }
}

/// Read a single newline-terminated line from `r`. Returns `None` on EOF or
/// timeout.
pub(crate) fn read_line<B: BufRead + ?Sized>(r: &mut B) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

/// Write a JSON value as a single newline-terminated line to `w`. The
/// receiver is taken by value so both `&UnixStream` (a `Write`) and
/// `&mut BufWriter<_>` can be passed.
pub(crate) fn write_line<W: Write>(w: W, v: &impl Serialize) -> std::io::Result<()> {
    let mut w = w;
    let line = serde_json::to_string(v)?;
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    let _ = w.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{FanState, Mode, Request, Response, State};

    fn sample_state() -> State {
        State {
            refresh_secs: 2.0,
            status: String::new(),
            fans: vec![FanState {
                id: "pwm1".into(),
                label: "Fan 1".into(),
                mode: Mode::Manual { percent: 50 },
                duty: Some(50.0),
                rpm: Some(1650),
                err: None,
                curve: vec![CurvePoint::new(30.0, 0.0), CurvePoint::new(75.0, 100.0)],
                default_mode: InitialMode::Auto,
            }],
            temps: vec![Temp {
                refname: "k10temp:temp1".into(),
                label: "Tctl".into(),
                source: "hwmon".into(),
                celsius: Some(70.0),
            }],
            sweeping: None,
            sweep: None,
        }
    }

    #[test]
    fn request_serializes_with_operation_tag() {
        let s = serde_json::to_string(&Request::SetMode {
            fan: "pwm1".into(),
            mode: Mode::Manual { percent: 50 },
        })
        .unwrap();
        assert_eq!(
            s,
            "{\"op\":\"set-mode\",\"fan\":\"pwm1\",\"mode\":{\"kind\":\"manual\",\"percent\":50}}"
        );
    }

    #[test]
    fn response_round_trips() {
        let s = serde_json::to_string(&Response::State {
            state: sample_state(),
        })
        .unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Response::State { .. }));
        let s = serde_json::to_string(&Response::Error {
            message: "nope".into(),
        })
        .unwrap();
        assert_eq!(s, "{\"op\":\"error\",\"message\":\"nope\"}");
    }

    #[test]
    fn mode_serializes_tagged() {
        assert_eq!(
            serde_json::to_string(&Mode::Auto).unwrap(),
            "{\"kind\":\"auto\"}"
        );
        assert_eq!(
            serde_json::to_string(&Mode::Full).unwrap(),
            "{\"kind\":\"full\"}"
        );
    }

    #[test]
    fn save_fan_config_request_round_trips() {
        let req = Request::SaveFanConfig {
            fan: "pwm1".into(),
            curve: vec![CurvePoint::new(30.0, 0.0), CurvePoint::new(75.0, 100.0)],
            default_mode: InitialMode::Off,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"op\":\"save-fan-config\""), "{s}");
        assert!(s.contains("\"default_mode\":\"off\""), "{s}");
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn sweep_request_round_trips() {
        let req = Request::Sweep {
            fan: "pwm1".into(),
            step: Some(5),
            settle: Some(1.5),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"op\":\"sweep\""), "{s}");
        assert!(s.contains("\"step\":5"), "{s}");
        assert!(s.contains("\"settle\":1.5"), "{s}");
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn state_with_a_sweep_report_round_trips() {
        let mut st = sample_state();
        st.sweeping = Some("pwm1".into());
        st.sweep = Some(crate::sweep::SweepReport {
            fan: "pwm1".into(),
            min_duty: Some(20),
            max_duty: Some(80),
            saturated: true,
            note: "pwm1: the fan spins between 20 % and 80 % duty".into(),
        });
        let s = serde_json::to_string(&st).unwrap();
        let back: State = serde_json::from_str(&s).unwrap();
        assert_eq!(back, st);
    }
}
