//! `fanctl` — a two-part application for Linux fan control and temperature
//! monitoring on any `hwmon`-equipped machine.
//!
//! * The **`fanctld`** daemon ([`daemon`]) owns the hardware: it reads
//!   temperatures, applies each fan's mode (curve-following, kernel auto,
//!   off, full, or a fixed manual duty), and serves clients over a Unix
//!   socket. It is meant to run as a service (e.g. a systemd unit) and
//!   reloads its config on `SIGHUP`.
//! * The **`fanctlui`** client ([`tui`]) renders the state served by the
//!   daemon and sends fan commands to it.
//!
//! The two talk the small JSON protocol in [`ipc`].

pub mod config;
pub mod ctl;
pub mod curve;
pub mod daemon;
pub mod error;
pub mod hwmon;
pub mod ipc;
pub mod summary;
pub mod sweep;
pub mod temps;
pub mod tui;
