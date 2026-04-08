use anyhow::{bail, Context, Result};

use crate::cli::ConfigCommand;
use crate::config::Config;
use crate::dirs;

pub fn run(command: &ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show { json: _ } => cmd_show(),
        ConfigCommand::Edit => cmd_edit(),
        ConfigCommand::Set { key, value } => cmd_set(key, value),
        ConfigCommand::Path => cmd_path(),
    }
}

fn cmd_show() -> Result<()> {
    let path = dirs::config_file()?;
    if !path.exists() {
        bail!(
            "No config file found at {}. Run `kcl init` to create one.",
            path.display()
        );
    }
    let config = Config::load(&path)?;
    // Config show always outputs JSON (it's the native format).
    let output = serde_json::to_string_pretty(&config).context("serializing config")?;
    println!("{}", output);
    Ok(())
}

fn cmd_edit() -> Result<()> {
    let path = dirs::config_file()?;
    if !path.exists() {
        bail!(
            "No config file found at {}. Run `kcl init` to create one.",
            path.display()
        );
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("failed to launch editor '{}'", editor))?;

    if !status.success() {
        bail!("editor exited with status {}", status);
    }
    Ok(())
}

fn cmd_set(key: &str, value: &str) -> Result<()> {
    let path = dirs::config_file()?;
    if !path.exists() {
        bail!(
            "No config file found at {}. Run `kcl init` to create one.",
            path.display()
        );
    }

    let mut config = Config::load(&path)?;

    match key {
        "clone_dir" => {
            config.clone_dir = value.to_string();
        }
        "default_harness" => {
            config.default_harness = value.to_string();
        }
        "default_timeout" => {
            let timeout: u64 = value
                .parse()
                .with_context(|| format!("invalid timeout value '{}': expected integer", value))?;
            config.default_timeout = timeout;
        }
        _ if key.starts_with("harnesses.") => {
            // Support dot-notation for harness properties:
            //   harnesses.<name>.command
            //   harnesses.<name>.prompt_mode
            let parts: Vec<&str> = key.splitn(4, '.').collect();
            if parts.len() < 3 {
                bail!(
                    "Invalid harness key '{}'. Expected harnesses.<name>.<property>",
                    key
                );
            }
            let harness_name = parts[1];
            let property = parts[2];

            let harness = config
                .harnesses
                .entry(harness_name.to_string())
                .or_insert_with(|| crate::config::HarnessConfig {
                    command: String::new(),
                    args: Vec::new(),
                    prompt_mode: crate::config::PromptMode::Arg,
                });

            match property {
                "command" => {
                    harness.command = value.to_string();
                }
                "prompt_mode" => {
                    harness.prompt_mode = serde_json::from_value(
                        serde_json::Value::String(value.to_string()),
                    )
                    .with_context(|| {
                        format!(
                            "invalid prompt_mode '{}': expected 'arg' or 'stdin'",
                            value
                        )
                    })?;
                }
                _ => {
                    bail!(
                        "Unknown harness property '{}'. Valid: command, prompt_mode",
                        property
                    );
                }
            }
        }
        _ => {
            bail!(
                "Unknown config key '{}'. Valid keys: clone_dir, default_harness, default_timeout, harnesses.<name>.<property>",
                key
            );
        }
    }

    config.save(&path)?;
    println!("Updated {} = {}", key, value);
    Ok(())
}

fn cmd_path() -> Result<()> {
    let path = dirs::config_file()?;
    println!("{}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use std::env;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn setup_test_config() -> (std::path::PathBuf, impl Drop) {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = env::temp_dir().join(format!(
            "kcl-config-test-{}-{}",
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let config_path = dir.join("config.json");
        let config = Config::default();
        config.save(&config_path).unwrap();

        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        (config_path, Cleanup(dir))
    }

    #[test]
    fn set_and_read_clone_dir() {
        let (path, _cleanup) = setup_test_config();

        let mut config = Config::load(&path).unwrap();
        config.clone_dir = "/tmp/new-packages".to_string();
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.clone_dir, "/tmp/new-packages");
    }

    #[test]
    fn set_and_read_default_timeout() {
        let (path, _cleanup) = setup_test_config();

        let mut config = Config::load(&path).unwrap();
        config.default_timeout = 300;
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.default_timeout, 300);
    }

    #[test]
    fn set_and_read_default_harness() {
        let (path, _cleanup) = setup_test_config();

        let mut config = Config::load(&path).unwrap();
        config.default_harness = "opencode".to_string();
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.default_harness, "opencode");
    }

    #[test]
    fn config_show_serializes_to_valid_json() {
        let config = Config::default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_object());
        assert_eq!(parsed["default_harness"], "claude");
        assert_eq!(parsed["default_timeout"], 120);
    }
}
