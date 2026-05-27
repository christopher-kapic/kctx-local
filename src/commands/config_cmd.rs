use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::ConfigCommand;
use crate::config::{Config, HarnessConfig, PromptMode, validate_harness_configured};
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

    if let Some(e) = &config.embeddings {
        writeln!(out, "embeddings:").unwrap();
        writeln!(out, "  provider:  {}", e.provider).unwrap();
        writeln!(out, "  model:     {}", e.model).unwrap();
    } else {
        writeln!(out, "embeddings:       (disabled)").unwrap();
    }

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

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "`kcl config edit` requires an interactive terminal. Edit {} directly or use `kcl config set`.",
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

    with_config_lock(&path, || apply_set(&path, key, value))?;
    println!("Updated {} = {}", key, value);
    Ok(())
}

fn apply_set(path: &Path, key: &str, value: &str) -> Result<()> {
    let mut config = Config::load(path)?;

    match key {
        "clone_dir" => {
            config.clone_dir = value.to_string();
        }
        "default_harness" => {
            // Validate before assignment so users can't set a default that
            // doesn't exist — otherwise the failure surfaces only later when
            // `kcl ask` tries to invoke it.
            validate_harness_configured(&config, value)?;
            config.default_harness = value.to_string();
        }
        "default_timeout" => {
            let timeout: u64 = value
                .parse()
                .with_context(|| format!("invalid timeout value `{}`: expected integer", value))?;
            crate::config::validate_timeout(timeout)?;
            config.default_timeout = timeout;
        }
        "embeddings.provider" => {
            // Setting the provider is the one entry point that may bring the
            // embeddings section into existence — we still need a default model
            // to validate against, so insert with `Default` here only.
            let p: crate::config::EmbeddingProvider = value.parse()?;
            let emb = config.embeddings.get_or_insert_with(Default::default);
            emb.provider = p;
        }
        "embeddings.model" => {
            // Refuse to silently enable the embeddings feature by inserting a
            // default OpenAI provider when the user only set the model. They
            // must opt in by setting `embeddings.provider` first; otherwise a
            // typo in the provider would never be caught.
            let Some(emb) = config.embeddings.as_mut() else {
                bail!(
                    "set `embeddings.provider` before `embeddings.model`. Example: `kcl config set embeddings.provider openai` then `kcl config set embeddings.model text-embedding-3-small`"
                );
            };
            if value.is_empty() {
                bail!("embeddings.model must not be empty");
            }
            emb.model = value.to_string();
        }
        _ if key.starts_with("harnesses.") => {
            // Support dot-notation for harness properties:
            //   harnesses.<name>.command
            //   harnesses.<name>.prompt_mode
            //   harnesses.<name>.args
            //   harnesses.<name>.default_model
            // The <name> may itself contain dots, so split off the final
            // segment as the property and treat everything before it as the name.
            let suffix = key.strip_prefix("harnesses.").unwrap();
            let (harness_name, property) = match suffix.rsplit_once('.') {
                Some((name, prop)) if !name.is_empty() && !prop.is_empty() => (name, prop),
                _ => bail!(
                    "Invalid harness key `{}`. Expected harnesses.<name>.<property>",
                    key
                ),
            };

            let harness = config
                .harnesses
                .entry(harness_name.to_string())
                .or_insert_with(|| crate::config::HarnessConfig {
                    command: String::new(),
                    args: Vec::new(),
                    prompt_mode: crate::config::PromptMode::Arg,
                    model_args: Vec::new(),
                    default_model: None,
                    prepared_args: vec![],
                });

            match property {
                "command" => {
                    if value.is_empty() {
                        bail!("harness `command` must not be empty");
                    }
                    harness.command = value.to_string();
                }
                "prompt_mode" => {
                    harness.prompt_mode = value.parse()?;
                }
                "args" => {
                    harness.args = if value.is_empty() {
                        Vec::new()
                    } else {
                        shlex::split(value)
                            .with_context(|| format!("failed to parse args value `{}`", value))?
                    };
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
                        "Unknown harness property `{}`. Valid: command, prompt_mode, args, default_model",
                        property
                    );
                }
            }

            if harness.command.is_empty() {
                bail!(
                    "harness `{}` has no command set. Set harnesses.{}.command first",
                    harness_name,
                    harness_name
                );
            }
        }
        _ => {
            bail!(
                "Unknown config key `{}`. Valid keys: clone_dir, default_harness, default_timeout, embeddings.provider, embeddings.model, harnesses.<name>.<property>",
                key
            );
        }
    }

    config.save(path)?;
    Ok(())
}

/// Execute `f` while holding an exclusive advisory lock on a sidecar lock file
/// beside the config. This serializes concurrent `kcl config set` invocations
/// so their read-modify-write sequences cannot interleave and silently drop
/// each other's changes.
///
/// The lock is taken on a sidecar (e.g. `.config.json.lock`) rather than on
/// the config file itself because [`Config::save`] replaces the config via
/// atomic rename, which changes the inode. Locking the sidecar — which is
/// never renamed — keeps all writers synchronized on a stable target.
fn with_config_lock<T>(config_path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    #[cfg(unix)]
    let _guard = acquire_config_lock(config_path)?;
    #[cfg(not(unix))]
    let _ = config_path;
    f()
}

#[cfg(unix)]
fn acquire_config_lock(config_path: &Path) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;

    let lock_path = config_lock_path(config_path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }

    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening lock file {}", lock_path.display()))?;

    loop {
        let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
        if rc == 0 {
            return Ok(lock_file);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(anyhow::anyhow!(
            "failed to acquire lock on {}: {}",
            lock_path.display(),
            err
        ));
    }
}

fn config_lock_path(config_path: &Path) -> PathBuf {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let filename = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    parent.join(format!(".{}.lock", filename))
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
                prepared_args: vec![],
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
                prepared_args: vec![],
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

    #[test]
    fn apply_set_default_harness_rejects_unconfigured_name() {
        use crate::config::{HarnessConfig, PromptMode};

        let (path, _cleanup) = setup_test_config();
        // Seed the config with one configured harness so the error message
        // exercises the "configured: claude" branch (not the "none configured"
        // fallback).
        let mut config = Config::load(&path).unwrap();
        config.harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
                prepared_args: vec![],
            },
        );
        config.save(&path).unwrap();

        let err = super::apply_set(&path, "default_harness", "nonexistent").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Unknown harness `nonexistent`"),
            "expected `Unknown harness` in error, got: {msg}"
        );

        // The config on disk must be unchanged after a rejected set.
        let after = Config::load(&path).unwrap();
        assert_eq!(after.default_harness, "claude");
    }

    #[test]
    fn apply_set_default_harness_accepts_configured_name() {
        use crate::config::{HarnessConfig, PromptMode};

        let (path, _cleanup) = setup_test_config();
        let mut config = Config::load(&path).unwrap();
        config.harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
                prepared_args: vec![],
            },
        );
        config.harnesses.insert(
            "opencode".to_string(),
            HarnessConfig {
                command: "opencode".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
                prepared_args: vec![],
            },
        );
        config.save(&path).unwrap();

        super::apply_set(&path, "default_harness", "opencode").unwrap();

        let after = Config::load(&path).unwrap();
        assert_eq!(after.default_harness, "opencode");
    }

    #[test]
    fn config_lock_path_is_hidden_sidecar() {
        use std::path::Path;
        let p = super::config_lock_path(Path::new("/home/u/.config/kcl/config.json"));
        assert_eq!(
            p,
            Path::new("/home/u/.config/kcl/.config.json.lock").to_path_buf()
        );
    }

    #[cfg(unix)]
    #[test]
    fn with_config_lock_serializes_concurrent_writers() {
        // Spawn two threads that each read-modify-write the same config under
        // the lock. Without locking, their interleaved load/save sequences
        // would race and one thread's change would be lost. With locking,
        // both changes must be present in the final config.
        use std::sync::Arc;
        use std::thread;

        let (path, _cleanup) = setup_test_config();
        let path = Arc::new(path);

        let handles: Vec<_> = (0..2)
            .map(|i| {
                let path = Arc::clone(&path);
                thread::spawn(move || {
                    for _ in 0..20 {
                        super::with_config_lock(&path, || {
                            let mut config = Config::load(&path).unwrap();
                            // Each thread writes to a different field.
                            if i == 0 {
                                let n: u64 = config
                                    .clone_dir
                                    .strip_prefix("count-")
                                    .and_then(|s| s.parse().ok())
                                    .unwrap_or(0);
                                // Simulate work between load and save to widen
                                // the race window.
                                std::thread::sleep(std::time::Duration::from_millis(1));
                                config.clone_dir = format!("count-{}", n + 1);
                            } else {
                                let n: u64 = config
                                    .default_harness
                                    .strip_prefix("h-")
                                    .and_then(|s| s.parse().ok())
                                    .unwrap_or(0);
                                std::thread::sleep(std::time::Duration::from_millis(1));
                                config.default_harness = format!("h-{}", n + 1);
                            }
                            config.save(&path)?;
                            Ok(())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let final_config = Config::load(&path).unwrap();
        assert_eq!(final_config.clone_dir, "count-20");
        assert_eq!(final_config.default_harness, "h-20");
    }
}
