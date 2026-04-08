use std::path::PathBuf;

use anyhow::{Context, Result};

const APP_NAME: &str = "kcl";

/// Returns the config directory for kcl.
/// Linux: ~/.config/kcl
/// macOS: ~/Library/Application Support/kcl
/// Windows: %APPDATA%\kcl
pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not determine config directory")?;
    Ok(base.join(APP_NAME))
}

/// Returns the data directory for kcl (SQLite database).
/// Linux: ~/.local/share/kcl
/// macOS: ~/Library/Application Support/kcl
/// Windows: %LOCALAPPDATA%\kcl
pub fn data_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().context("could not determine data directory")?;
    Ok(base.join(APP_NAME))
}

/// Returns the state directory for kcl (conversation logs).
/// Linux: ~/.local/state/kcl
/// macOS: ~/Library/Application Support/kcl (falls back to data_dir)
/// Windows: %LOCALAPPDATA%\kcl (falls back to data_dir)
pub fn state_dir() -> Result<PathBuf> {
    let base = dirs::state_dir()
        .or_else(dirs::data_dir)
        .context("could not determine state directory")?;
    Ok(base.join(APP_NAME))
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
