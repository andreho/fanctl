//! Reading and writing the Linux `hwmon` sysfs interface (`/sys/class/hwmon`).
//!
//! This is the single source of both temperatures and fan (PWM) control, so
//! `fanctl` no longer needs to fork `sensors` to drive fans.

use std::fs;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

/// Default location of the hwmon sysfs tree.
pub const SYS_HWMON: &str = "/sys/class/hwmon";

/// A single hwmon chip (a `hwmonN` directory).
#[derive(Debug, Clone)]
pub struct Hwmon {
    /// The value of the chip's `name` file (e.g. `it8792`, `k10temp`).
    pub name: String,
    /// The directory name (e.g. `hwmon5`), which is unique even when several
    /// chips share the same `name`.
    pub dir: String,
    /// The path to the `hwmonN` directory.
    pub path: PathBuf,
}

/// Discover all hwmon chips under `base`, in a stable order.
pub fn discover(base: &Path) -> Vec<Hwmon> {
    let mut out = Vec::new();
    let mut entries: Vec<std::fs::DirEntry> = match fs::read_dir(base) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return out,
    };
    entries.sort_by_key(|e| e.file_name());

    for e in &entries {
        let p = e.path();
        let dir = p.file_name().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        if !dir.starts_with("hwmon") || !p.is_dir() {
            continue;
        }
        let name = fs::read_to_string(p.join("name"))
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| dir.trim_start_matches("hwmon").to_string());
        out.push(Hwmon {
            name,
            dir: dir.clone(),
            path: p.clone(),
        });
    }
    out
}

/// Resolve a config chip reference to a specific chip. The ref may be a chip
/// name (e.g. `it8792`) or a directory (e.g. `hwmon2`). Names are tried first
/// (for friendliness); a name mapping to several chips is disambiguated by the
/// directory.
pub fn resolve<'a>(chips: &'a [Hwmon], ref_: &str) -> Option<&'a Hwmon> {
    let by_name: Vec<&Hwmon> = chips.iter().filter(|c| c.name == ref_).collect();
    if by_name.len() == 1 {
        return Some(by_name[0]);
    }
    if let Some(c) = chips.iter().find(|c| c.dir == ref_) {
        return Some(c);
    }
    None
}

impl Hwmon {
    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Read a file from this chip, returning the trimmed string.
    pub fn read(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.file(name)).ok().map(|s| s.trim().to_string())
    }

    pub fn read_i64(&self, name: &str) -> Option<i64> {
        self.read(name).and_then(|s| s.parse().ok())
    }

    /// Write a value to a file on this chip.
    pub fn write(&self, name: &str, val: &str) -> std::io::Result<()> {
        fs::write(self.file(name), val)
    }

    /// Numbers of the PWM channels this chip exposes (`pwm1`, `pwm2`, ...).
    pub fn pwm_numbers(&self) -> Vec<u32> {
        self.list_numbers("pwm")
    }

    /// Numbers of the temperature sensors (`temp1`, `temp2`, ...).
    pub fn temp_numbers(&self) -> Vec<u32> {
        self.list_numbers("temp")
    }

    /// Numbers of the fan tachometer sensors (`fan1`, `fan2`, ...).
    pub fn fan_numbers(&self) -> Vec<u32> {
        self.list_numbers("fan")
    }

    fn list_numbers(&self, prefix: &str) -> Vec<u32> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(&self.path) else {
            return out;
        };
        for e in rd.filter_map(|e| e.ok()) {
            let n: String = e.file_name().to_string_lossy().into_owned();
            if let Some(rest) = n.strip_prefix(prefix) {
                // The number is the leading run of digits, e.g. `1` from
                // `pwm1`, `1_enable`, or `1_input`. It must be followed by
                // nothing or an underscore so we don't misread `temp10` as `1`.
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(x) = digits.parse::<u32>() {
                    let after = &rest[digits.len()..];
                    if after.is_empty() || after.starts_with('_') {
                        out.push(x);
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    // ---- Fan (PWM) control -------------------------------------------------

    /// The current raw PWM value for channel `n`, if readable.
    pub fn pwm_raw(&self, n: u32) -> Option<u32> {
        self.read_i64(&format!("pwm{n}")).and_then(|v| v.try_into().ok())
    }

    /// The current PWM enable/mode bits for channel `n`.
    ///
    /// The meaning of the value is **driver-specific**: on the it87 family
    /// (Linux `it87` driver, e.g. the it8792) the accepted values are
    /// `0` = off, `1` = manual, `2` = chip auto; other chips (thinkfan-style)
    /// add `3` = chip auto and `4` = max. We only ever write `1` and `2`
    /// (off is implemented as a 0% manual duty, see [`Self::set_pwm_off`]),
    /// which every driver in practice understands (at worst `2` means
    /// "auto" on all of them).
    pub fn pwm_enable(&self, n: u32) -> Option<u32> {
        self.read_i64(&format!("pwm{n}_enable"))
            .and_then(|v| v.try_into().ok())
    }

    /// Set the PWM to a manual duty. `pct` is 0..=100, scaled to the chip's
    /// raw range (`0..=pwm_max`).
    pub fn set_pwm_manual(&self, n: u32, pct: f64, pwm_max: u32) -> std::io::Result<()> {
        self.write(&format!("pwm{n}_enable"), "1")?;
        let raw = ((pct.clamp(0.0, 100.0) / 100.0) * pwm_max as f64).round() as u32;
        self.write(&format!("pwm{n}"), &raw.to_string())
    }

    /// Let the chip's own auto-algorithm drive the PWM (`pwmN_enable = 2`).
    ///
    /// On the it87 family `2` is the chip's "automatic" mode (the kernel
    /// driver's only auto bit); other hwmon drivers use `2` or `3` for the
    /// same thing, so this is a safe, portable encoding.
    pub fn set_pwm_auto(&self, n: u32) -> std::io::Result<()> {
        self.write(&format!("pwm{n}_enable"), "2")
    }

    /// Drive the PWM at full speed: manual mode (`pwmN_enable = 1`) at the
    /// chip's maximum value. Some chips expose a dedicated "max" mode bit,
    /// but the it87 driver has none (values > 2 are a hard -EINVAL), so
    /// 100% manual is the portable way to get full speed.
    pub fn set_pwm_full(&self, n: u32, pwm_max: u32) -> std::io::Result<()> {
        self.set_pwm_manual(n, 100.0, pwm_max)
    }

    /// Stop the PWM.
    ///
    /// This writes a 0% *manual* duty, not `pwmN_enable = 0`. On the it87
    /// family (FEAT_FANCTL_ONOFF chips such as the IT8792) the kernel
    /// driver's "off" path switches the fan to on/off mode but deliberately
    /// keeps it running at 100% ("make sure the fan is on when in on/off
    /// mode"), so `0` is not an off at all — it sounds exactly like
    /// "full". A 0% manual duty stops the fan on that chip, and a 0% duty
    /// is likewise a stopped fan on the other drivers we support.
    pub fn set_pwm_off(&self, n: u32, pwm_max: u32) -> std::io::Result<()> {
        self.set_pwm_manual(n, 0.0, pwm_max)
    }

    /// The current duty percentage of the PWM, computed from its raw value and
    /// the optional `pwm{n}_min`/`pwm{n}_max` files (falling back to `0..=pwm_max`).
    pub fn pwm_percent(&self, n: u32, pwm_max: u32) -> Option<f64> {
        let raw = self.pwm_raw(n)?;
        let (min, max) = (self.pwm_range_min(n), self.pwm_range_max(n, pwm_max));
        if max <= min {
            return None;
        }
        let f = (raw as f64 - min as f64) / (max as f64 - min as f64);
        Some((f * 100.0).clamp(0.0, 100.0))
    }

    fn pwm_range_min(&self, n: u32) -> u32 {
        self.read_i64(&format!("pwm{n}_min"))
            .and_then(|v| v.try_into().ok())
            .unwrap_or(0)
    }

    fn pwm_range_max(&self, n: u32, default: u32) -> u32 {
        self.read_i64(&format!("pwm{n}_max"))
            .and_then(|v| v.try_into().ok())
            .unwrap_or(default)
    }
}
