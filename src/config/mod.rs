//! The `fanctl` configuration: model, (de)serialization, and auto-generation.

pub mod generate;
pub mod types;
pub mod yaml;

use std::path::Path;

use crate::error::Error;
use crate::hwmon;

pub use types::Config;

/// Resolve the default config path: `$XDG_CONFIG_HOME/fanctl/fanctl.yaml`,
/// falling back to `~/.config/fanctl/fanctl.yaml`.
pub fn default_path() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let xdg = xdg.trim_end_matches('/');
        if !xdg.is_empty() {
            return std::path::Path::new(xdg).join("fanctl").join("fanctl.yaml");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    std::path::Path::new(home.as_str())
        .join(".config")
        .join("fanctl")
        .join("fanctl.yaml")
}

/// Load the config at `path`, or (on first run) probe the hardware and
/// generate a starting point there.
pub fn load_or_generate(path: &Path) -> Result<Config, Error> {
    if !path.exists() {
        let cfg = generate::generate(Path::new(hwmon::SYS_HWMON));
        yaml::save(path, &cfg)?;
        eprintln!("fanctl: generated config at {}", path.display());
        eprintln!("fanctl: edit the curves, then re-run.\n");
        return Ok(cfg);
    }
    yaml::load(path)
}
