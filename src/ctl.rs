//! The shared one-shot `--set` command, offered by both binaries:
//! `fanctld --set pwm1=99` writes the chip directly and exits, while
//! `fanctlui --set pwm1=99` sends the same mode to a running `fanctld`
//! over its socket and exits. This module only parses the command line;
//! each binary applies the result its own way.

use crate::ipc::Mode;

/// One parsed `--set FAN=MODE` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Set {
    /// The fan to address: its `pwmN` id as known to the daemon (or to the
    /// config, for the one-shot `fanctld` form).
    pub fan: String,
    /// The mode to apply to it.
    pub mode: Mode,
}

/// Parse one `--set` argument, `FAN=MODE`.
///
/// `MODE` is either a duty in percent (`0..=100`, e.g. `99`), or one of the
/// keywords `auto`, `off`, `full` (the same choices the TUI keys `a` / `o` /
/// `f` apply).
pub fn parse_set(s: &str) -> Result<Set, String> {
    let (fan, mode_s) = s
        .split_once('=')
        .ok_or_else(|| format!("bad --set argument {s:?}: expected FAN=MODE, e.g. pwm1=99"))?;
    let fan = fan.trim();
    if fan.is_empty() {
        return Err(format!(
            "bad --set argument {s:?}: the fan id must not be empty"
        ));
    }
    let mode = match mode_s.trim() {
        "auto" => Mode::Auto,
        "off" => Mode::Off,
        "full" => Mode::Full,
        _ => match mode_s.trim().parse::<u32>() {
            Ok(p) if p <= 100 => Mode::Manual { percent: p as u8 },
            Ok(p) => {
                return Err(format!(
                    "bad --set value {mode_s:?}: the percent must be in 0..=100 (got {p})"
                ))
            }
            Err(_) => {
                return Err(format!(
                    "bad --set value {mode_s:?}: expected a percent (0-100) or auto|off|full"
                ))
            }
        },
    };
    Ok(Set {
        fan: fan.into(),
        mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_values() {
        let s = parse_set("pwm1=99").unwrap();
        assert_eq!(s.fan, "pwm1");
        assert_eq!(s.mode, Mode::Manual { percent: 99 });
        assert_eq!(
            parse_set("pwm1=0").unwrap().mode,
            Mode::Manual { percent: 0 }
        );
        assert_eq!(
            parse_set("pwm2=100").unwrap().mode,
            Mode::Manual { percent: 100 }
        );
    }

    #[test]
    fn keyword_values() {
        assert_eq!(parse_set("pwm1=auto").unwrap().mode, Mode::Auto);
        assert_eq!(parse_set("pwm1=off").unwrap().mode, Mode::Off);
        assert_eq!(parse_set("pwm1=full").unwrap().mode, Mode::Full);
    }

    #[test]
    fn out_of_range_and_garbage_are_rejected() {
        for bad in [
            "pwm1=101",
            "pwm1=1000",
            "pwm1=",
            "=99",
            "pwm1",
            "pwm1=abc",
            "pwm1=-5",
        ] {
            assert!(parse_set(bad).is_err(), "{bad:?} must be rejected");
        }
        let err = parse_set("pwm1=101").unwrap_err();
        assert!(err.contains("0..=100"), "{err}");
    }
}
