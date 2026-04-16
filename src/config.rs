use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level kcl configuration, stored at ~/.config/kcl/config.json.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Directory where git packages are cloned. Tilde-expanded.
    pub clone_dir: String,

    /// Which harness to use when not overridden.
    pub default_harness: String,

    /// Seconds before killing harness subprocess.
    #[serde(default = "default_timeout")]
    pub default_timeout: u64,

    /// Map of harness name to harness definition.
    #[serde(default)]
    pub harnesses: HashMap<String, HarnessConfig>,
}

/// A harness definition — an external coding agent invoked as a subprocess.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessConfig {
    /// Executable name or path.
    pub command: String,

    /// Argument list. `{prompt}` is replaced when prompt_mode is "arg".
    #[serde(default)]
    pub args: Vec<String>,

    /// How the prompt is delivered: "arg" (replace {prompt} in args) or "stdin".
    #[serde(default = "default_prompt_mode")]
    pub prompt_mode: PromptMode,

    /// Optional template for forwarding `--model` to the harness. When the
    /// user supplies `--model X` to `kcl ask`, these args are appended to the
    /// harness invocation with `{model}` replaced by `X`. If empty, the
    /// harness does not support model selection and `--model` is silently
    /// ignored for it.
    ///
    /// Examples:
    /// - `["--model", "{model}"]` — flag + value as separate args
    /// - `["--model={model}"]` — single combined arg
    /// - `["-m", "{model}"]` — short flag
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_args: Vec<String>,

    /// Optional model identifier used when the user does not pass `--model`
    /// to `kcl ask`. Forwarded through `model_args` exactly as if the user
    /// had supplied it on the command line. Ignored when `model_args` is
    /// empty (the harness has no way to receive a model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    Arg,
    Stdin,
}

pub const MIN_TIMEOUT: u64 = 1;

fn default_timeout() -> u64 {
    120
}

pub fn validate_timeout(seconds: u64) -> Result<()> {
    if seconds < MIN_TIMEOUT {
        anyhow::bail!(
            "timeout must be at least {} second(s), got {}",
            MIN_TIMEOUT,
            seconds
        );
    }
    Ok(())
}

impl FromStr for PromptMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "arg" => Ok(PromptMode::Arg),
            "stdin" => Ok(PromptMode::Stdin),
            _ => anyhow::bail!("invalid prompt_mode '{}': expected 'arg' or 'stdin'", s),
        }
    }
}

fn default_prompt_mode() -> PromptMode {
    PromptMode::Arg
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clone_dir: "~/src/kcl-packages".to_string(),
            default_harness: "claude".to_string(),
            default_timeout: default_timeout(),
            harnesses: HashMap::new(),
        }
    }
}

impl Config {
    /// Load config from the given path. Returns an error if the file doesn't
    /// exist or can't be parsed.
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if contents.trim().is_empty() {
            return Ok(Self::default());
        }
        let config: Config = serde_json::from_str(&contents)
            .with_context(|| format!("parsing {}", path.display()))?;
        validate_timeout(config.default_timeout)?;
        Ok(config)
    }

    /// Load config from the default config file location.
    /// Returns the default config if the file doesn't exist.
    pub fn load_or_default() -> Result<Self> {
        let path = crate::paths::config_file()?;
        if path.exists() {
            Self::load(&path)
        } else {
            Ok(Self::default())
        }
    }

    /// Save config to the given path, creating parent directories as needed.
    /// Uses atomic temp-file + rename to prevent corruption on crash.
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("config path has no parent directory")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;

        let json = serde_json::to_string_pretty(self).context("serializing config")?;

        let mut tmp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("creating temp file in {}", parent.display()))?;
        tmp.write_all(json.as_bytes())
            .context("writing config to temp file")?;
        tmp.flush().context("flushing config temp file")?;
        tmp.persist(path)
            .with_context(|| format!("persisting config to {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_full_config() {
        let json = r#"{
            "clone_dir": "~/src/kcl-packages",
            "default_harness": "claude",
            "default_timeout": 120,
            "harnesses": {
                "claude": {
                    "command": "claude",
                    "args": ["-p", "{prompt}", "--allowedTools", "Bash,Read,Write,Edit,Glob,Grep", "--bare"],
                    "prompt_mode": "arg"
                },
                "openclaude": {
                    "command": "openclaude",
                    "args": ["-p", "--output-format", "text", "{prompt}"],
                    "prompt_mode": "arg"
                },
                "copilot": {
                    "command": "copilot",
                    "args": ["-p", "{prompt}", "--silent", "--allow-all-paths"],
                    "prompt_mode": "arg"
                },
                "pi": {
                    "command": "pi",
                    "args": ["-p", "{prompt}"],
                    "prompt_mode": "arg"
                },
                "opencode": {
                    "command": "opencode",
                    "args": ["run", "{prompt}"],
                    "prompt_mode": "arg"
                },
                "codex": {
                    "command": "codex",
                    "args": ["-p", "{prompt}"],
                    "prompt_mode": "arg"
                },
                "custom": {
                    "command": "my-agent",
                    "args": ["--query"],
                    "prompt_mode": "stdin"
                }
            }
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.clone_dir, "~/src/kcl-packages");
        assert_eq!(config.default_harness, "claude");
        assert_eq!(config.default_timeout, 120);
        assert_eq!(config.harnesses.len(), 7);

        // Verify claude harness
        let claude = &config.harnesses["claude"];
        assert_eq!(claude.command, "claude");
        assert_eq!(claude.prompt_mode, PromptMode::Arg);
        assert_eq!(
            claude.args,
            vec![
                "-p",
                "{prompt}",
                "--allowedTools",
                "Bash,Read,Write,Edit,Glob,Grep",
                "--bare"
            ]
        );

        // Verify stdin prompt mode
        let custom = &config.harnesses["custom"];
        assert_eq!(custom.prompt_mode, PromptMode::Stdin);
    }

    #[test]
    fn deserialize_minimal_config() {
        let json = r#"{
            "clone_dir": "/tmp/packages",
            "default_harness": "claude"
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.clone_dir, "/tmp/packages");
        assert_eq!(config.default_harness, "claude");
        assert_eq!(config.default_timeout, 120);
        assert!(config.harnesses.is_empty());
    }

    #[test]
    fn default_config_has_sane_values() {
        let config = Config::default();
        assert_eq!(config.clone_dir, "~/src/kcl-packages");
        assert_eq!(config.default_harness, "claude");
        assert_eq!(config.default_timeout, 120);
    }

    #[test]
    fn roundtrip_serialize_deserialize() {
        let mut config = Config::default();
        config.harnesses.insert(
            "test".to_string(),
            HarnessConfig {
                command: "test-agent".to_string(),
                args: vec!["-p".to_string(), "{prompt}".to_string()],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
            },
        );

        let json = serde_json::to_string_pretty(&config).unwrap();
        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn validate_timeout_rejects_zero() {
        let err = validate_timeout(0).unwrap_err();
        assert!(err.to_string().contains("at least 1"));
    }

    #[test]
    fn validate_timeout_accepts_minimum() {
        validate_timeout(1).unwrap();
    }

    #[test]
    fn validate_timeout_accepts_large_value() {
        validate_timeout(3600).unwrap();
    }

    #[test]
    fn load_rejects_zero_timeout() {
        let dir = std::env::temp_dir().join("kcl-test-zero-timeout");
        let path = dir.join("config.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{"clone_dir":"/tmp","default_harness":"claude","default_timeout":0}"#,
        )
        .unwrap();

        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("at least 1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_atomic_no_temp_files_left() {
        let dir = std::env::temp_dir().join("kcl-test-atomic-save");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");

        let config = Config::default();
        config.save(&path).unwrap();

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "only config.json should remain");
        assert_eq!(entries[0].file_name(), "config.json");

        let loaded = Config::load(&path).unwrap();
        assert_eq!(config, loaded);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_mode_from_str_arg() {
        assert_eq!("arg".parse::<PromptMode>().unwrap(), PromptMode::Arg);
    }

    #[test]
    fn prompt_mode_from_str_stdin() {
        assert_eq!("stdin".parse::<PromptMode>().unwrap(), PromptMode::Stdin);
    }

    #[test]
    fn prompt_mode_from_str_invalid() {
        let err = "bogus".parse::<PromptMode>().unwrap_err();
        assert!(err.to_string().contains("invalid prompt_mode 'bogus'"));
        assert!(err.to_string().contains("'arg' or 'stdin'"));
    }

    #[test]
    fn load_empty_file_returns_default() {
        let dir = std::env::temp_dir().join("kcl-test-empty-config");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        std::fs::write(&path, "").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config, Config::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_whitespace_only_file_returns_default() {
        let dir = std::env::temp_dir().join("kcl-test-whitespace-config");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        std::fs::write(&path, "  \n\t\n  ").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config, Config::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_load_config() {
        let dir = std::env::temp_dir().join("kcl-test-config");
        let path = dir.join("config.json");

        let mut config = Config::default();
        config.harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec!["-p".to_string(), "{prompt}".to_string()],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
            },
        );

        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(config, loaded);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
