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

    /// Optional extra args appended to the harness invocation **only** when a
    /// prepared orientation map is injected into a `kcl ask` (never for
    /// `kcl prepare` itself, and never for a plain map-less `ask`). These let
    /// the harness config impose a mechanical exploration ceiling (e.g.
    /// `["--max-turns", "8"]` or a restricted `["--allowedTools", "Read,Grep"]`)
    /// that caps cost even if the model ignores the prompt's "trust the map"
    /// guidance. The same placeholder substitution applied to `args` /
    /// `model_args` (`{prompt}` in `arg` prompt mode, `{model}` when a model is
    /// resolved) is applied here so combined-form args keep working.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prepared_args: Vec<String>,

    /// Whether to append the inline `kcl explore` toolkit summary to the
    /// prompt for this harness. Defaults to `true` so the harness sees the
    /// navigation primitives without any config change; can be set to `false`
    /// for harnesses where the extra block is noise (very small context
    /// windows, harnesses that already get the info another way).
    #[serde(default = "default_true")]
    pub inject_explore_toolkit: bool,
}

fn default_true() -> bool {
    true
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            prompt_mode: PromptMode::Arg,
            model_args: Vec::new(),
            default_model: None,
            prepared_args: Vec::new(),
            inject_explore_toolkit: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    #[default]
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

/// Verify that `name` matches a harness defined in `config.harnesses`.
///
/// Used by every code path that records a harness name (package overrides,
/// the default_harness top-level key, manifest imports) so that an invalid
/// name fails fast at write time with a helpful list of configured options,
/// rather than blowing up later when `kcl ask` tries to invoke it.
pub fn validate_harness_configured(config: &Config, name: &str) -> Result<()> {
    if config.harnesses.contains_key(name) {
        return Ok(());
    }
    let mut configured: Vec<&str> = config.harnesses.keys().map(String::as_str).collect();
    configured.sort();
    let valid = if configured.is_empty() {
        "(none configured; run `kcl init` to add harnesses)".to_string()
    } else {
        configured.join(", ")
    };
    anyhow::bail!("Unknown harness `{name}`. Configured harnesses: {valid}");
}

impl FromStr for PromptMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "arg" => Ok(PromptMode::Arg),
            "stdin" => Ok(PromptMode::Stdin),
            _ => anyhow::bail!("invalid prompt_mode `{}`: expected `arg` or `stdin`", s),
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

    /// Resolve the configured clone_dir, expanding a leading `~`.
    ///
    /// Shared by `kcl packages` (add/remove) and `kcl prune` so the
    /// "is this path a kcl-managed clone inside clone_dir" check is computed
    /// the same way everywhere. Returns an error if `clone_dir` starts with
    /// `~` but the home directory cannot be determined.
    pub fn resolved_clone_dir(&self) -> Result<std::path::PathBuf> {
        if let Some(rest) = self.clone_dir.strip_prefix("~/") {
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("cannot expand `~`: home directory not found"))?;
            Ok(home.join(rest))
        } else if self.clone_dir == "~" {
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("cannot expand `~`: home directory not found"))?;
            Ok(home)
        } else {
            Ok(std::path::PathBuf::from(&self.clone_dir))
        }
    }

    /// Save config to the given path, creating parent directories as needed.
    /// Uses atomic temp-file + rename to prevent corruption on crash.
    pub fn save(&self, path: &Path) -> Result<()> {
        validate_timeout(self.default_timeout)?;

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
                prepared_args: vec![],
                inject_explore_toolkit: true,
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
    fn validate_harness_accepts_configured_name() {
        let mut config = Config::default();
        config.harnesses.insert(
            "my-custom".to_string(),
            HarnessConfig {
                command: "my-agent".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
                prepared_args: vec![],
                inject_explore_toolkit: true,
            },
        );
        validate_harness_configured(&config, "my-custom").unwrap();
    }

    #[test]
    fn validate_harness_rejects_unconfigured_name() {
        let mut config = Config::default();
        config.harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
                prepared_args: vec![],
                inject_explore_toolkit: true,
            },
        );
        let err = validate_harness_configured(&config, "nonexistent").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Unknown harness `nonexistent`"));
        assert!(msg.contains("claude"));
    }

    #[test]
    fn validate_harness_reports_when_none_configured() {
        let config = Config::default();
        let err = validate_harness_configured(&config, "anything").unwrap_err();
        assert!(err.to_string().contains("none configured"));
    }

    #[test]
    fn resolved_clone_dir_expands_tilde_prefix() {
        let mut config = Config::default();
        config.clone_dir = "~/src/kcl-packages".to_string();
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            config.resolved_clone_dir().unwrap(),
            home.join("src/kcl-packages")
        );
    }

    #[test]
    fn resolved_clone_dir_expands_bare_tilde() {
        let mut config = Config::default();
        config.clone_dir = "~".to_string();
        let home = dirs::home_dir().unwrap();
        assert_eq!(config.resolved_clone_dir().unwrap(), home);
    }

    #[test]
    fn resolved_clone_dir_passes_through_absolute_path() {
        let mut config = Config::default();
        config.clone_dir = "/tmp/clones".to_string();
        assert_eq!(
            config.resolved_clone_dir().unwrap(),
            std::path::PathBuf::from("/tmp/clones")
        );
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
        assert!(err.to_string().contains("invalid prompt_mode `bogus`"));
        assert!(err.to_string().contains("`arg` or `stdin`"));
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
                prepared_args: vec![],
                inject_explore_toolkit: true,
            },
        );

        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(config, loaded);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deserialize_harness_without_prepared_args_defaults_empty() {
        // Existing configs that predate `prepared_args` must still load and
        // get an empty vec (no behavior change for them).
        let json = r#"{
            "clone_dir": "/tmp/p",
            "default_harness": "claude",
            "harnesses": {
                "claude": {
                    "command": "claude",
                    "args": ["-p", "{prompt}"],
                    "prompt_mode": "arg"
                }
            }
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let claude = &config.harnesses["claude"];
        assert!(
            claude.prepared_args.is_empty(),
            "missing prepared_args must default to empty"
        );
    }

    #[test]
    fn deserialize_harness_with_prepared_args() {
        let json = r#"{
            "clone_dir": "/tmp/p",
            "default_harness": "claude",
            "harnesses": {
                "claude": {
                    "command": "claude",
                    "args": ["-p", "{prompt}"],
                    "prompt_mode": "arg",
                    "prepared_args": ["--max-turns", "8", "--allowedTools", "Read,Grep"]
                }
            }
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let claude = &config.harnesses["claude"];
        assert_eq!(
            claude.prepared_args,
            vec!["--max-turns", "8", "--allowedTools", "Read,Grep"]
        );
    }

    #[test]
    fn roundtrip_with_prepared_args() {
        let mut config = Config::default();
        config.harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec!["-p".to_string(), "{prompt}".to_string()],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
                prepared_args: vec!["--max-turns".to_string(), "8".to_string()],
                inject_explore_toolkit: true,
            },
        );
        let json = serde_json::to_string_pretty(&config).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn prepared_args_omitted_from_json_when_empty() {
        // `skip_serializing_if = "Vec::is_empty"` keeps existing config files
        // byte-stable: an empty `prepared_args` is not written out.
        let mut config = Config::default();
        config.harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec!["-p".to_string()],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
                prepared_args: vec![],
                inject_explore_toolkit: true,
            },
        );
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("prepared_args"),
            "empty prepared_args must be skipped in serialized output"
        );
    }
}
