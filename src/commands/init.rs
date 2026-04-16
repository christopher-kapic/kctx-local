use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::Cli;
use crate::config::{Config, HarnessConfig, PromptMode};

/// Known harness definitions — built-in templates for common coding agents.
fn known_harnesses() -> Vec<(&'static str, HarnessConfig)> {
    vec![
        (
            "claude",
            HarnessConfig {
                command: "claude".to_string(),
                args: vec![
                    "-p".to_string(),
                    "{prompt}".to_string(),
                    "--allowedTools".to_string(),
                    "Bash,Read,Write,Edit,Glob,Grep".to_string(),
                    "--bare".to_string(),
                ],
                prompt_mode: PromptMode::Arg,
                model_args: vec!["--model".to_string(), "{model}".to_string()],
                default_model: None,
            },
        ),
        (
            "copilot",
            HarnessConfig {
                command: "copilot".to_string(),
                args: vec![
                    "-p".to_string(),
                    "{prompt}".to_string(),
                    "--silent".to_string(),
                    "--allow-all-paths".to_string(),
                    "--allow-all".to_string(),
                ],
                prompt_mode: PromptMode::Arg,
                // copilot uses `=`-style: `--model=<name>` (e.g.
                // `--model=claude-sonnet-4.6` or `--model=gpt-5.2`).
                model_args: vec!["--model={model}".to_string()],
                default_model: None,
            },
        ),
        (
            "pi",
            HarnessConfig {
                command: "pi".to_string(),
                args: vec!["-p".to_string(), "{prompt}".to_string()],
                prompt_mode: PromptMode::Arg,
                // Pi accepts `--model <pattern>` (e.g. `gpt-4o-mini`,
                // `openai/gpt-4o`, `sonnet:high`).
                model_args: vec!["--model".to_string(), "{model}".to_string()],
                default_model: None,
            },
        ),
        (
            "opencode",
            HarnessConfig {
                command: "opencode".to_string(),
                args: vec!["run".to_string(), "{prompt}".to_string()],
                prompt_mode: PromptMode::Arg,
                // opencode expects `-m provider/model` — the user supplies
                // the full provider/model string as the --model value
                // (e.g. `anthropic/claude-sonnet-4-20250514`).
                model_args: vec!["-m".to_string(), "{model}".to_string()],
                default_model: None,
            },
        ),
        (
            "codex",
            HarnessConfig {
                command: "codex".to_string(),
                // codex's non-interactive entrypoint is the `exec`
                // subcommand. Prompt is a positional arg.
                // `--skip-git-repo-check` allows running outside a git
                // repo; `--ephemeral` avoids persisting session files.
                // `codex exec` already defaults to `AskForApproval::Never`
                // in headless mode (see codex-rs/exec/src/lib.rs ~L370 and
                // codex-rs/exec/src/cli.rs), so no approval-policy override
                // is needed.
                args: vec![
                    "exec".to_string(),
                    "{prompt}".to_string(),
                    "--skip-git-repo-check".to_string(),
                    "--ephemeral".to_string(),
                ],
                prompt_mode: PromptMode::Arg,
                // codex accepts `-m <model>` / `--model <model>`.
                model_args: vec!["-m".to_string(), "{model}".to_string()],
                default_model: None,
            },
        ),
        (
            "goose",
            HarnessConfig {
                command: "goose".to_string(),
                // goose non-interactive invocation is `goose run -t <prompt>`.
                // `--no-session` skips persisting a session file so repeated
                // ask invocations don't litter the filesystem.
                args: vec![
                    "run".to_string(),
                    "-t".to_string(),
                    "{prompt}".to_string(),
                    "--no-session".to_string(),
                ],
                prompt_mode: PromptMode::Arg,
                // goose accepts `--model <name>` on `run`. If your
                // goose build instead requires `GOOSE_MODEL` env var,
                // clear this and set the env var ambient.
                model_args: vec!["--model".to_string(), "{model}".to_string()],
                default_model: None,
            },
        ),
    ]
}

/// Returns the canonical list of harness names, derived from [`known_harnesses`].
pub(crate) fn known_harness_names() -> Vec<&'static str> {
    known_harnesses()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Scan PATH for known harnesses, returning names of those found.
fn detect_harnesses() -> Vec<String> {
    known_harness_names()
        .into_iter()
        .filter(|name| which::which(name).is_ok())
        .map(|name| name.to_string())
        .collect()
}

/// Select the default harness based on what was detected.
/// Returns (selected_name, was_user_prompted).
fn select_default_harness(detected: &[String], non_interactive: bool) -> Result<String> {
    match detected.len() {
        0 => {
            eprintln!("Warning: no known harnesses found in PATH. Defaulting to `claude`.");
            eprintln!(
                "Install a supported harness or configure one manually with `kcl config set`."
            );
            Ok("claude".to_string())
        }
        1 => {
            eprintln!("Auto-selected harness: {}", detected[0]);
            Ok(detected[0].clone())
        }
        _ => {
            if non_interactive {
                // Pick first detected
                eprintln!(
                    "Multiple harnesses found: {}. Auto-selecting `{}`.",
                    detected.join(", "),
                    detected[0]
                );
                Ok(detected[0].clone())
            } else {
                // Prompt user
                eprintln!("Multiple harnesses found:");
                for (i, name) in detected.iter().enumerate() {
                    eprintln!("  [{}] {}", i + 1, name);
                }
                eprint!("Select default harness [1]: ");
                io::stderr().flush()?;

                let stdin = io::stdin();
                let line = stdin.lock().lines().next();
                let input = match line {
                    Some(Ok(s)) => s.trim().to_string(),
                    _ => String::new(),
                };

                if input.is_empty() {
                    Ok(detected[0].clone())
                } else {
                    let idx: usize = input
                        .parse::<usize>()
                        .context("invalid selection")?
                        .checked_sub(1)
                        .context("selection out of range")?;
                    detected.get(idx).cloned().context("selection out of range")
                }
            }
        }
    }
}

/// Prompt user for clone directory (or use default in non-interactive mode).
fn get_clone_dir(non_interactive: bool) -> Result<String> {
    let default_dir = "~/src/kcl-packages";

    if non_interactive {
        return Ok(default_dir.to_string());
    }

    eprint!("Clone directory [{}]: ", default_dir);
    io::stderr().flush()?;

    let stdin = io::stdin();
    let line = stdin.lock().lines().next();
    let input = match line {
        Some(Ok(s)) => s.trim().to_string(),
        _ => String::new(),
    };

    if input.is_empty() {
        Ok(default_dir.to_string())
    } else {
        Ok(input)
    }
}

/// Build the harness map for detected harnesses.
fn build_harness_map(detected: &[String]) -> HashMap<String, HarnessConfig> {
    let known: HashMap<&str, HarnessConfig> = known_harnesses().into_iter().collect();
    let mut map = HashMap::new();

    for name in detected {
        if let Some(config) = known.get(name.as_str()) {
            map.insert(name.clone(), config.clone());
        }
    }

    // If no harnesses were detected, still include claude as the default template
    if map.is_empty()
        && let Some(claude) = known.get("claude")
    {
        map.insert("claude".to_string(), claude.clone());
    }

    map
}

/// Merge new harness entries into an existing config without overwriting.
fn merge_config(
    existing: &mut Config,
    new_harnesses: HashMap<String, HarnessConfig>,
    clone_dir: &str,
    default_harness: &str,
) {
    // Only update clone_dir if it's still the default (hasn't been customized)
    if existing.clone_dir == "~/src/kcl-packages" {
        existing.clone_dir = clone_dir.to_string();
    }

    // Add new harnesses that don't already exist
    for (name, config) in new_harnesses {
        existing.harnesses.entry(name).or_insert(config);
    }

    // Only update default harness if not already set to a valid harness
    if !existing.harnesses.contains_key(&existing.default_harness) {
        existing.default_harness = default_harness.to_string();
    }
}

/// Generate shell completion files for bash, zsh, and fish.
fn generate_completions(completions_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(completions_dir).with_context(|| {
        format!(
            "creating completions directory: {}",
            completions_dir.display()
        )
    })?;

    let shells = [
        (Shell::Bash, "kcl.bash"),
        (Shell::Zsh, "_kcl"),
        (Shell::Fish, "kcl.fish"),
    ];

    for (shell, filename) in &shells {
        let path = completions_dir.join(filename);
        let mut file =
            std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let mut cmd = Cli::command();
        generate(*shell, &mut cmd, "kcl", &mut file);
    }

    Ok(())
}

pub fn run(non_interactive: bool) -> Result<()> {
    // 1. Detect harnesses
    let detected = detect_harnesses();

    // 2. Select default harness
    let default_harness = select_default_harness(&detected, non_interactive)?;

    // 3. Get clone directory
    let clone_dir = get_clone_dir(non_interactive)?;

    // 4. Build harness map
    let harness_map = build_harness_map(&detected);

    // 5. Load or create config, merging if it already exists
    let config_path = crate::paths::config_file()?;
    let config_existed = config_path.exists();

    let mut config = if config_existed {
        Config::load(&config_path)?
    } else {
        Config::default()
    };

    if config_existed {
        let new_harness_names: Vec<String> = harness_map
            .keys()
            .filter(|k| !config.harnesses.contains_key(*k))
            .cloned()
            .collect();
        merge_config(&mut config, harness_map, &clone_dir, &default_harness);
        if !new_harness_names.is_empty() {
            eprintln!("Merged new harnesses: {}", new_harness_names.join(", "));
        }
    } else {
        config.clone_dir = clone_dir.clone();
        config.default_harness = default_harness.clone();
        config.harnesses = harness_map;
    }

    config.save(&config_path)?;

    // 6. Create database with schema
    let db_path = crate::paths::db_file()?;
    let _conn = crate::db::open(&db_path)?;

    // 7. Generate shell completions
    let completions_dir = crate::paths::config_dir()?.join("completions");
    generate_completions(&completions_dir)?;

    // 8. Print summary
    eprintln!();
    eprintln!("kcl initialized successfully!");
    eprintln!();
    if config_existed {
        eprintln!("  Config:       {} (merged)", config_path.display());
    } else {
        eprintln!("  Config:       {} (created)", config_path.display());
    }
    eprintln!("  Database:     {}", db_path.display());
    eprintln!("  Completions:  {}", completions_dir.display());
    eprintln!("  Clone dir:    {}", config.clone_dir);
    eprintln!("  Default:      {}", config.default_harness);
    eprintln!();

    if detected.is_empty() {
        eprintln!("  Harnesses:    (none detected)");
    } else {
        eprintln!("  Harnesses:");
        for name in &detected {
            eprintln!("    - {} (found)", name);
        }
    }

    let all_names = known_harness_names();
    let not_found: Vec<&str> = all_names
        .iter()
        .filter(|n| !detected.contains(&n.to_string()))
        .copied()
        .collect();
    if !not_found.is_empty() {
        eprintln!();
        eprintln!("  Not found:    {}", not_found.join(", "));
    }

    eprintln!();
    eprintln!("Get started:");
    eprintln!("  kcl packages add <name> --path /path/to/codebase");
    eprintln!("  kcl ask <name> \"How does the auth system work?\"");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_harnesses_has_all_six() {
        let harnesses = known_harnesses();
        assert_eq!(harnesses.len(), 6);
        let names: Vec<&str> = harnesses.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"claude"));
        assert!(names.contains(&"copilot"));
        assert!(names.contains(&"pi"));
        assert!(names.contains(&"opencode"));
        assert!(names.contains(&"codex"));
        assert!(names.contains(&"goose"));
    }

    #[test]
    fn codex_uses_hardened_exec_flags() {
        let harnesses = known_harnesses();
        let (_, codex) = harnesses.iter().find(|(n, _)| *n == "codex").unwrap();
        assert_eq!(codex.args[0], "exec");
        assert!(codex.args.contains(&"{prompt}".to_string()));
        assert!(codex.args.contains(&"--skip-git-repo-check".to_string()));
        assert!(codex.args.contains(&"--ephemeral".to_string()));
        // `codex exec` already defaults to AskForApproval::Never in headless
        // mode, so we deliberately do NOT pass `-c approval_policy=...`.
        assert!(!codex.args.contains(&"approval_policy=never".to_string()));
        assert!(!codex.args.contains(&"--full-auto".to_string()));
    }

    #[test]
    fn copilot_passes_allow_all() {
        let harnesses = known_harnesses();
        let (_, copilot) = harnesses.iter().find(|(n, _)| *n == "copilot").unwrap();
        assert!(copilot.args.contains(&"--allow-all".to_string()));
        assert!(copilot.args.contains(&"--allow-all-paths".to_string()));
    }

    #[test]
    fn goose_uses_run_subcommand() {
        let harnesses = known_harnesses();
        let (_, goose) = harnesses.iter().find(|(n, _)| *n == "goose").unwrap();
        assert_eq!(goose.args[0], "run");
        assert!(goose.args.contains(&"-t".to_string()));
        assert!(goose.args.contains(&"{prompt}".to_string()));
        assert!(goose.args.contains(&"--no-session".to_string()));
    }

    #[test]
    fn detect_harnesses_returns_vec() {
        // Just ensure it runs without panic — actual results depend on PATH
        let _detected = detect_harnesses();
    }

    #[test]
    fn select_default_none_detected() {
        let detected: Vec<String> = vec![];
        let result = select_default_harness(&detected, true).unwrap();
        assert_eq!(result, "claude");
    }

    #[test]
    fn select_default_one_detected() {
        let detected = vec!["opencode".to_string()];
        let result = select_default_harness(&detected, true).unwrap();
        assert_eq!(result, "opencode");
    }

    #[test]
    fn select_default_multiple_non_interactive() {
        let detected = vec!["claude".to_string(), "copilot".to_string()];
        let result = select_default_harness(&detected, true).unwrap();
        assert_eq!(result, "claude");
    }

    #[test]
    fn build_harness_map_with_detected() {
        let detected = vec!["claude".to_string(), "opencode".to_string()];
        let map = build_harness_map(&detected);
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("claude"));
        assert!(map.contains_key("opencode"));
        assert_eq!(map["claude"].command, "claude");
        assert_eq!(map["opencode"].command, "opencode");
    }

    #[test]
    fn build_harness_map_empty_includes_claude() {
        let detected: Vec<String> = vec![];
        let map = build_harness_map(&detected);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("claude"));
    }

    #[test]
    fn merge_config_adds_new_harnesses() {
        let mut existing = Config {
            clone_dir: "/custom/dir".to_string(),
            default_harness: "claude".to_string(),
            default_timeout: 120,
            harnesses: {
                let mut m = HashMap::new();
                m.insert(
                    "claude".to_string(),
                    HarnessConfig {
                        command: "claude".to_string(),
                        args: vec!["-p".to_string(), "{prompt}".to_string()],
                        prompt_mode: PromptMode::Arg,
                        model_args: vec![],
                        default_model: None,
                    },
                );
                m
            },
        };

        let mut new_harnesses = HashMap::new();
        new_harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude-new".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
            },
        );
        new_harnesses.insert(
            "opencode".to_string(),
            HarnessConfig {
                command: "opencode".to_string(),
                args: vec!["run".to_string(), "{prompt}".to_string()],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
            },
        );

        merge_config(&mut existing, new_harnesses, "~/new-dir", "opencode");

        // Should NOT overwrite existing claude entry
        assert_eq!(existing.harnesses["claude"].command, "claude");
        // Should add new opencode entry
        assert!(existing.harnesses.contains_key("opencode"));
        assert_eq!(existing.harnesses["opencode"].command, "opencode");
        // Should NOT overwrite custom clone_dir
        assert_eq!(existing.clone_dir, "/custom/dir");
        // default_harness should stay since claude is a valid key
        assert_eq!(existing.default_harness, "claude");
    }

    #[test]
    fn merge_config_updates_default_if_invalid() {
        let mut existing = Config {
            clone_dir: "~/src/kcl-packages".to_string(),
            default_harness: "nonexistent".to_string(),
            default_timeout: 120,
            harnesses: HashMap::new(),
        };

        let mut new_harnesses = HashMap::new();
        new_harnesses.insert(
            "opencode".to_string(),
            HarnessConfig {
                command: "opencode".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
            },
        );

        merge_config(
            &mut existing,
            new_harnesses,
            "~/src/kcl-packages",
            "opencode",
        );

        assert_eq!(existing.default_harness, "opencode");
    }

    #[test]
    fn generate_completions_creates_files() {
        let dir = std::env::temp_dir().join("kcl-test-completions");
        let _ = std::fs::remove_dir_all(&dir);

        generate_completions(&dir).unwrap();

        assert!(dir.join("kcl.bash").exists());
        assert!(dir.join("_kcl").exists());
        assert!(dir.join("kcl.fish").exists());

        // Verify files are non-empty
        assert!(
            !std::fs::read_to_string(dir.join("kcl.bash"))
                .unwrap()
                .is_empty()
        );
        assert!(
            !std::fs::read_to_string(dir.join("_kcl"))
                .unwrap()
                .is_empty()
        );
        assert!(
            !std::fs::read_to_string(dir.join("kcl.fish"))
                .unwrap()
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_interactive_init_creates_config_and_db() {
        // Use temp dirs to avoid touching real config
        let tmp = std::env::temp_dir().join("kcl-test-init");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config_path = tmp.join("config.json");
        let db_path = tmp.join("kcl.db");
        let completions_dir = tmp.join("completions");

        // Build config
        let detected = detect_harnesses();
        let default_harness = select_default_harness(&detected, true).unwrap();
        let harness_map = build_harness_map(&detected);

        let config = Config {
            default_harness,
            harnesses: harness_map,
            ..Default::default()
        };
        config.save(&config_path).unwrap();

        // Create DB
        let _conn = crate::db::open(&db_path).unwrap();

        // Generate completions
        generate_completions(&completions_dir).unwrap();

        // Verify
        assert!(config_path.exists());
        assert!(db_path.exists());
        assert!(completions_dir.join("kcl.bash").exists());

        let loaded = Config::load(&config_path).unwrap();
        assert!(!loaded.harnesses.is_empty());
        assert_eq!(loaded.clone_dir, "~/src/kcl-packages");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn init_twice_merges_config() {
        let tmp = std::env::temp_dir().join("kcl-test-init-merge");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config_path = tmp.join("config.json");

        // First "init" — create config with just claude
        let mut config = Config {
            default_harness: "claude".to_string(),
            ..Default::default()
        };
        config.harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "my-custom-claude".to_string(),
                args: vec!["--custom".to_string()],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
            },
        );
        config.clone_dir = "/my/custom/dir".to_string();
        config.save(&config_path).unwrap();

        // Second "init" — merge new harnesses
        let mut loaded = Config::load(&config_path).unwrap();
        let mut new_harnesses = HashMap::new();
        new_harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec!["-p".to_string(), "{prompt}".to_string()],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
            },
        );
        new_harnesses.insert(
            "opencode".to_string(),
            HarnessConfig {
                command: "opencode".to_string(),
                args: vec!["run".to_string(), "{prompt}".to_string()],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
            },
        );

        merge_config(&mut loaded, new_harnesses, "~/src/kcl-packages", "claude");
        loaded.save(&config_path).unwrap();

        // Verify merge behavior
        let final_config = Config::load(&config_path).unwrap();
        // Existing claude should NOT be overwritten
        assert_eq!(final_config.harnesses["claude"].command, "my-custom-claude");
        // New opencode should be added
        assert!(final_config.harnesses.contains_key("opencode"));
        // Custom clone_dir should be preserved
        assert_eq!(final_config.clone_dir, "/my/custom/dir");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
