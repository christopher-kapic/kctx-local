use std::path::PathBuf;

use anyhow::{Context, Result};

const APP_NAME: &str = "kcl";

/// Returns the config directory for kcl.
///
/// kcl uses XDG-style config paths on every platform (not the native macOS
/// `~/Library/Application Support` location), so the config lives at:
/// - `$XDG_CONFIG_HOME/kcl` if `XDG_CONFIG_HOME` is set
/// - `~/.config/kcl` otherwise
pub fn config_dir() -> Result<PathBuf> {
    let base = xdg_dir_or_home_relative("XDG_CONFIG_HOME", &[".config"])?;
    Ok(base.join(APP_NAME))
}

/// Returns the data directory for kcl (SQLite database).
///
/// kcl uses XDG-style data paths on every platform (not the native macOS
/// `~/Library/Application Support` location), so the database lives at:
/// - `$XDG_DATA_HOME/kcl` if `XDG_DATA_HOME` is set
/// - `~/.local/share/kcl` otherwise
pub fn data_dir() -> Result<PathBuf> {
    let base = xdg_dir_or_home_relative("XDG_DATA_HOME", &[".local", "share"])?;
    Ok(base.join(APP_NAME))
}

/// Returns the state directory for kcl (conversation logs).
///
/// kcl uses XDG-style state paths on every platform (not the native macOS
/// `~/Library/Application Support` location), so logs live under:
/// - `$XDG_STATE_HOME/kcl` if `XDG_STATE_HOME` is set
/// - `~/.local/state/kcl` otherwise
pub fn state_dir() -> Result<PathBuf> {
    let base = xdg_dir_or_home_relative("XDG_STATE_HOME", &[".local", "state"])?;
    Ok(base.join(APP_NAME))
}

/// Resolves an XDG base directory: returns `$VAR` if set to an absolute path,
/// otherwise `$HOME` joined with the given fallback components.
fn xdg_dir_or_home_relative(var: &str, fallback: &[&str]) -> Result<PathBuf> {
    if let Some(val) = std::env::var_os(var) {
        let path = PathBuf::from(val);
        if path.is_absolute() {
            return Ok(path);
        }
        // Per the XDG spec, a non-absolute value is invalid; fall through.
    }
    let mut home = dirs::home_dir().context("could not determine home directory")?;
    for component in fallback {
        home.push(component);
    }
    Ok(home)
}

/// Returns the log directory for conversation logs.
/// <state_dir>/logs
pub fn log_dir() -> Result<PathBuf> {
    Ok(state_dir()?.join("logs"))
}

/// Returns the path to the config file.
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

/// Returns the path to the SQLite database.
pub fn db_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("kcl.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_ends_with_kcl() {
        let dir = config_dir().unwrap();
        assert_eq!(dir.file_name().unwrap(), "kcl");
    }

    #[test]
    fn data_dir_ends_with_kcl() {
        let dir = data_dir().unwrap();
        assert_eq!(dir.file_name().unwrap(), "kcl");
    }

    #[test]
    fn state_dir_ends_with_kcl() {
        let dir = state_dir().unwrap();
        assert_eq!(dir.file_name().unwrap(), "kcl");
    }

    #[test]
    fn log_dir_ends_with_logs() {
        let dir = log_dir().unwrap();
        assert_eq!(dir.file_name().unwrap(), "logs");
        assert_eq!(dir.parent().unwrap().file_name().unwrap(), "kcl");
    }

    #[test]
    fn config_file_is_json() {
        let path = config_file().unwrap();
        assert_eq!(path.file_name().unwrap(), "config.json");
    }

    #[test]
    fn db_file_is_sqlite() {
        let path = db_file().unwrap();
        assert_eq!(path.file_name().unwrap(), "kcl.db");
    }
}
