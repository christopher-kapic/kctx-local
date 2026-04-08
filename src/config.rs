use std::collections::HashMap;
use std::path::Path;

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    Arg,
    Stdin,
}

fn default_timeout() -> u64 {
    120
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
        let config: Config = serde_json::from_str(&contents)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(config)
    }

    /// Load config from the default config file location.
    /// Returns the default config if the file doesn't exist.
    pub fn load_or_default() -> Result<Self> {
        let path = crate::dirs::config_file()?;
        if path.exists() {
            Self::load(&path)
        } else {
            Ok(Self::default())
        }
    }

    /// Save config to the given path, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serializing config")?;
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
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
            },
        );

        let json = serde_json::to_string_pretty(&config).unwrap();
        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
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
            },
        );

        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(config, loaded);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
