//! The `fanctl` configuration: model, (de)serialization, and auto-generation.

pub mod generate;
pub mod types;
pub mod yaml;

/// Resolve the default config path: `$XDG_CONFIG_HOME/fanctl/fanctl.yaml`,
/// falling back to `~/.config/fanctl/fanctl.yaml`.
pub fn default_path() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let xdg = xdg.trim_end_matches('/');
        return std::path::Path::new(xdg).join("fanctl").join("fanctl.yaml");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    std::path::Path::new(home.as_str()).join(".config").join("fanctl").join("fanctl.yaml")
}
