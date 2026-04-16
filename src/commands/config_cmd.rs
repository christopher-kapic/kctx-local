use anyhow::{Context, Result, bail};

use crate::cli::ConfigCommand;
use crate::config::{Config, HarnessConfig, PromptMode};
use crate::paths;

pub fn run(command: &ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show { json } => cmd_show(*json),
        ConfigCommand::Edit => cmd_edit(),
        ConfigCommand::Set { key, value } => cmd_set(key, value),
        ConfigCommand::Path => cmd_path(),
    }
}

fn cmd_show(json: bool) -> Result<()> {
    let path = paths::config_file()?;
    if !path.exists() {
        bail!(
            "No config file found at {}. Run `kcl init` to create one.",
            path.display()
        );
    }
    let config = Config::load(&path)?;

    if json {
        let output = serde_json::to_string_pretty(&config).context("serializing config")?;
        println!("{}", output);
        return Ok(());
    }

    print!("{}", format_human(&config));
    Ok(())
}

fn format_human(config: &Config) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    writeln!(out, "clone_dir:        {}", config.clone_dir).unwrap();
    writeln!(out, "default_harness:  {}", config.default_harness).unwrap();
    writeln!(out, "default_timeout:  {}", config.default_timeout).unwrap();

    if config.harnesses.is_empty() {
        writeln!(out, "harnesses:        (none configured)").unwrap();
        return out;
    }

    writeln!(out, "harnesses:").unwrap();
    let mut names: Vec<&String> = config.harnesses.keys().collect();
    names.sort();
    for name in names {
        let h: &HarnessConfig = &config.harnesses[name];
        let marker = if name == &config.default_harness {
            " (default)"
        } else {
            ""
        };
        writeln!(out, "  {}{}", name, marker).unwrap();
        writeln!(out, "    command:       {}", h.command).unwrap();
        writeln!(
            out,
            "    prompt_mode:   {}",
            prompt_mode_str(&h.prompt_mode)
        )
        .unwrap();
        writeln!(out, "    args:          {}", format_args(&h.args)).unwrap();
        if !h.model_args.is_empty() {
            writeln!(out, "    model_args:    {}", h.model_args.join(" ")).unwrap();
        }
        if let Some(model) = &h.default_model {
            writeln!(out, "    default_model: {}", model).unwrap();
        }
    }
    out
}

fn prompt_mode_str(mode: &PromptMode) -> &'static str {
    match mode {
        PromptMode::Arg => "arg",
        PromptMode::Stdin => "stdin",
    }
}

fn format_args(args: &[String]) -> String {
    if args.is_empty() {
        "-".to_string()
    } else {
        args.join(" ")
    }
}

fn cmd_edit() -> Result<()> {
    let path = paths::config_file()?;
    if !path.exists() {
        bail!(
            "No config file found at {}. Run `kcl init` to create one.",
            path.display()
        );
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let parts = shlex::split(&editor)
        .with_context(|| format!("failed to parse EDITOR value `{}`", editor))?;
    let (program, extra_args) = parts
        .split_first()
        .with_context(|| format!("EDITOR value `{}` is empty", editor))?;
    let status = std::process::Command::new(program)
        .args(extra_args)
        .arg(&path)
        .status()
        .with_context(|| format!("failed to launch editor `{}`", editor))?;

    if !status.success() {
        bail!("editor exited with status {}", status);
    }
    Ok(())
}

fn cmd_set(key: &str, value: &str) -> Result<()> {
    let path = paths::config_file()?;
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
                .with_context(|| format!("invalid timeout value `{}`: expected integer", value))?;
            crate::config::validate_timeout(timeout)?;
            config.default_timeout = timeout;
        }
        _ if key.starts_with("harnesses.") => {
            // Support dot-notation for harness properties:
            //   harnesses.<name>.command
            //   harnesses.<name>.prompt_mode
            let parts: Vec<&str> = key.splitn(4, '.').collect();
            if parts.len() < 3 {
                bail!(
                    "Invalid harness key `{}`. Expected harnesses.<name>.<property>",
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
                    model_args: Vec::new(),
                    default_model: None,
                });

            match property {
                "command" => {
                    harness.command = value.to_string();
                }
                "prompt_mode" => {
                    harness.prompt_mode = value.parse()?;
                }
                "default_model" => {
                    // Empty string clears the default model.
                    harness.default_model = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                _ => {
                    bail!(
                        "Unknown harness property `{}`. Valid: command, prompt_mode, default_model",
                        property
                    );
                }
            }
        }
        _ => {
            bail!(
                "Unknown config key `{}`. Valid keys: clone_dir, default_harness, default_timeout, harnesses.<name>.<property>",
                key
            );
        }
    }

    config.save(&path)?;
    println!("Updated {} = {}", key, value);
    Ok(())
}

fn cmd_path() -> Result<()> {
    let path = paths::config_file()?;
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
        let dir = env::temp_dir().join(format!("kcl-config-test-{}-{}", std::process::id(), id));
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
    fn set_timeout_rejects_zero() {
        let (path, _cleanup) = setup_test_config();

        let mut config = Config::load(&path).unwrap();
        config.default_timeout = 0;
        // Bypass save validation to test cmd_set path
        let json = serde_json::to_string_pretty(&config).unwrap();
        std::fs::write(&path, json).unwrap();

        // Simulate what cmd_set does: parse + validate
        let value = "0";
        let timeout: u64 = value.parse().unwrap();
        let result = crate::config::validate_timeout(timeout);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least 1"));
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

    #[test]
    fn format_human_shows_top_level_keys() {
        let config = Config::default();
        let out = super::format_human(&config);
        assert!(out.contains("clone_dir:        ~/src/kcl-packages"));
        assert!(out.contains("default_harness:  claude"));
        assert!(out.contains("default_timeout:  120"));
        assert!(out.contains("harnesses:        (none configured)"));
        // Human output must NOT look like JSON.
        assert!(!out.trim_start().starts_with('{'));
    }

    #[test]
    fn format_human_shows_harnesses_with_default_marker() {
        use crate::config::{HarnessConfig, PromptMode};
        let mut config = Config::default();
        config.harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec!["-p".to_string(), "{prompt}".to_string()],
                prompt_mode: PromptMode::Arg,
                model_args: vec!["--model".to_string(), "{model}".to_string()],
                default_model: Some("claude-sonnet-4-6".to_string()),
            },
        );
        config.harnesses.insert(
            "pi".to_string(),
            HarnessConfig {
                command: "pi".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Stdin,
                model_args: vec![],
                default_model: None,
            },
        );

        let out = super::format_human(&config);
        assert!(out.contains("claude (default)"));
        assert!(out.contains("pi\n"));
        assert!(!out.contains("pi (default)"));
        assert!(out.contains("command:       claude"));
        assert!(out.contains("prompt_mode:   arg"));
        assert!(out.contains("prompt_mode:   stdin"));
        assert!(out.contains("args:          -p {prompt}"));
        assert!(out.contains("args:          -"));
        assert!(out.contains("model_args:    --model {model}"));
        assert!(out.contains("default_model: claude-sonnet-4-6"));
    }
}
