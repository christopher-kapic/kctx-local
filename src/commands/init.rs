use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::Cli;
use crate::config::{
    Config, EmbeddingConfig, EmbeddingProvider, HarnessConfig, PromptMode,
    default_embedding_model_for_provider, validate_embedding_config,
};

/// Bundled arguments for `kcl init`. Keeps the command surface tidy as new
/// embedding-related flags pile up while still letting `main.rs` build the
/// struct from clap-parsed fields. All embedding-related fields default to
/// "feature off" so existing call sites need no changes when adding new ones.
pub struct InitArgs<'a> {
    pub non_interactive: bool,
    pub enable_embeddings: bool,
    pub embedding_provider: Option<&'a str>,
    pub embedding_model: Option<&'a str>,
}

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
                prepared_args: vec![],
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
                prepared_args: vec![],
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
                prepared_args: vec![],
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
                prepared_args: vec![],
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
                prepared_args: vec![],
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
                prepared_args: vec![],
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

/// Select the default harness based on what was detected. When `existing_default`
/// is provided and matches a detected harness, it becomes the proposed default
/// in the prompt; otherwise the first detected harness is proposed.
fn select_default_harness(
    detected: &[String],
    non_interactive: bool,
    existing_default: Option<&str>,
) -> Result<String> {
    let existing_idx = existing_default.and_then(|d| detected.iter().position(|n| n == d));

    match detected.len() {
        0 => {
            let fallback = existing_default.unwrap_or("claude");
            eprintln!(
                "Warning: no known harnesses found in PATH. Defaulting to `{}`.",
                fallback
            );
            eprintln!(
                "Install a supported harness or configure one manually with `kcl config set`."
            );
            Ok(fallback.to_string())
        }
        1 => {
            eprintln!("Auto-selected harness: {}", detected[0]);
            Ok(detected[0].clone())
        }
        _ => {
            let suggested_idx = existing_idx.unwrap_or(0);
            if non_interactive {
                let chosen = &detected[suggested_idx];
                eprintln!(
                    "Multiple harnesses found: {}. Auto-selecting `{}`.",
                    detected.join(", "),
                    chosen
                );
                Ok(chosen.clone())
            } else {
                eprintln!("Multiple harnesses found:");
                for (i, name) in detected.iter().enumerate() {
                    eprintln!("  [{}] {}", i + 1, name);
                }
                eprint!("Select default harness [{}]: ", suggested_idx + 1);
                io::stderr().flush()?;

                let stdin = io::stdin();
                let line = stdin.lock().lines().next();
                let input = match line {
                    Some(Ok(s)) => s.trim().to_string(),
                    _ => String::new(),
                };

                if input.is_empty() {
                    Ok(detected[suggested_idx].clone())
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

/// Prompt user for clone directory. The proposed default is `existing` when
/// provided, otherwise the built-in default.
fn get_clone_dir(non_interactive: bool, existing: Option<&str>) -> Result<String> {
    let default_dir = existing.unwrap_or("~/src/kcl-packages");

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

/// Merge new harness entries into an existing config. `clone_dir` and
/// `default_harness` are taken as the user's chosen values (the caller is
/// expected to have proposed the existing config's values as defaults during
/// the prompt). New harness templates are added without clobbering any
/// existing entries the user may have customized.
fn merge_config(
    existing: &mut Config,
    new_harnesses: HashMap<String, HarnessConfig>,
    clone_dir: &str,
    default_harness: &str,
) {
    existing.clone_dir = clone_dir.to_string();
    existing.default_harness = default_harness.to_string();

    for (name, config) in new_harnesses {
        existing.harnesses.entry(name).or_insert(config);
    }
}

/// Returns the env-var name the embeddings client expects for `provider`.
/// Used by `resolve_embedding_choice` to (a) print the export instruction
/// shown to the user and (b) probe for an already-set key so we can warn
/// when it's missing in non-interactive flows.
fn env_var_for_provider(provider: EmbeddingProvider) -> &'static str {
    match provider {
        EmbeddingProvider::Openai => "OPENAI_API_KEY",
        EmbeddingProvider::Openrouter => "OPENROUTER_API_KEY",
    }
}

/// Result of the embeddings portion of `kcl init`.
///
/// We need three states, not two:
/// - preserve the existing config when the user didn't engage with embeddings
/// - write a new embeddings section
/// - explicitly remove the existing embeddings section
enum EmbeddingChoice {
    Preserve,
    Set(EmbeddingConfig),
    Disable,
}

/// Resolve the user's embeddings choice for `kcl init`.
///
/// In non-interactive mode the three flags are authoritative: `--enable-embeddings`
/// (with optional `--embedding-provider` / `--embedding-model`) wires up the
/// section using sensible defaults. Passing `--embedding-provider` or
/// `--embedding-model` also enables embeddings implicitly.
///
/// In interactive mode we ask the user, biasing the default toward whatever
/// they already have configured. If they answer yes we prompt for provider
/// and model (defaulting to per-provider sane choices) and print a one-line
/// `export <VAR>=...` hint pointing at the env var the embeddings client
/// will look up at call time. The API key itself is never read or stored
/// here — only the provider + model.
fn resolve_embedding_choice(
    non_interactive: bool,
    enable_embeddings: bool,
    embedding_provider: Option<&str>,
    embedding_model: Option<&str>,
    existing: Option<EmbeddingConfig>,
) -> Result<EmbeddingChoice> {
    let embeddings_requested =
        enable_embeddings || embedding_provider.is_some() || embedding_model.is_some();

    if non_interactive {
        if !embeddings_requested {
            return Ok(EmbeddingChoice::Preserve);
        }
        let provider = match embedding_provider {
            Some(s) => s.parse::<EmbeddingProvider>()?,
            None => existing
                .as_ref()
                .map(|e| e.provider)
                .unwrap_or(EmbeddingProvider::Openai),
        };
        let model = embedding_model
            .map(|s| s.to_string())
            .or_else(|| {
                existing
                    .as_ref()
                    .filter(|e| e.provider == provider)
                    .map(|e| e.model.clone())
            })
            .unwrap_or_else(|| default_embedding_model_for_provider(provider).to_string());
        let cfg = EmbeddingConfig { provider, model };
        validate_embedding_config(&cfg)?;
        warn_if_api_key_missing(provider);
        eprintln!(
            "Embeddings enabled: provider `{}`, model `{}`. Set `{}` in your shell to activate.",
            cfg.provider,
            cfg.model,
            env_var_for_provider(provider)
        );
        return Ok(EmbeddingChoice::Set(cfg));
    }

    // Interactive mode.
    if embeddings_requested {
        // Caller already opted in via flag; skip the y/N prompt but still
        // prompt for the missing pieces.
        let provider = resolve_provider_interactive(embedding_provider, existing.as_ref())?;
        let model = resolve_model_interactive(embedding_model, provider, existing.as_ref())?;
        let cfg = EmbeddingConfig { provider, model };
        validate_embedding_config(&cfg)?;
        print_export_hint(&cfg);
        return Ok(EmbeddingChoice::Set(cfg));
    }

    // No flag — ask. Default to existing setting if any, otherwise N.
    let default_yes = existing.is_some();
    let default_label = if default_yes { "Y/n" } else { "y/N" };
    eprint!(
        "Enable semantic question memory with embeddings? [{}]: ",
        default_label
    );
    io::stderr().flush()?;
    let stdin = io::stdin();
    let answered = stdin
        .lock()
        .lines()
        .next()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let answer = answered.trim();
    let yes = if answer.is_empty() {
        default_yes
    } else {
        matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes")
    };
    if !yes {
        return Ok(EmbeddingChoice::Disable);
    }
    let provider = resolve_provider_interactive(None, existing.as_ref())?;
    let model = resolve_model_interactive(None, provider, existing.as_ref())?;
    let cfg = EmbeddingConfig { provider, model };
    validate_embedding_config(&cfg)?;
    print_export_hint(&cfg);
    Ok(EmbeddingChoice::Set(cfg))
}

/// Prompt the user to choose an embedding provider, falling back through:
/// 1. an explicit flag value (if Some — parsed via FromStr),
/// 2. the existing config's provider (proposed as default),
/// 3. OpenAI (the safe default).
fn resolve_provider_interactive(
    flag: Option<&str>,
    existing: Option<&EmbeddingConfig>,
) -> Result<EmbeddingProvider> {
    if let Some(v) = flag {
        return v.parse::<EmbeddingProvider>();
    }
    let default = existing
        .map(|e| e.provider)
        .unwrap_or(EmbeddingProvider::Openai);
    let options = [EmbeddingProvider::Openai, EmbeddingProvider::Openrouter];
    eprintln!("Embedding provider:");
    for (i, p) in options.iter().enumerate() {
        eprintln!("  [{}] {}", i + 1, p);
    }
    let default_idx = options.iter().position(|p| *p == default).unwrap_or(0);
    eprint!("Select provider [{}]: ", default_idx + 1);
    io::stderr().flush()?;
    let line = io::stdin().lock().lines().next();
    let input = match line {
        Some(Ok(s)) => s.trim().to_string(),
        _ => String::new(),
    };
    if input.is_empty() {
        return Ok(options[default_idx]);
    }
    // Accept either the menu index or a literal provider name.
    if let Ok(idx) = input.parse::<usize>() {
        let i = idx
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("selection out of range"))?;
        return options
            .get(i)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("selection out of range"));
    }
    input.parse::<EmbeddingProvider>()
}

/// Prompt the user for the model identifier with a sensible per-provider
/// default. The default tracks the user's existing model setting if one was
/// already configured.
fn resolve_model_interactive(
    flag: Option<&str>,
    provider: EmbeddingProvider,
    existing: Option<&EmbeddingConfig>,
) -> Result<String> {
    if let Some(v) = flag {
        if v.trim().is_empty() {
            anyhow::bail!("`--embedding-model` must not be empty");
        }
        let model = v.trim().to_string();
        validate_embedding_config(&EmbeddingConfig {
            provider,
            model: model.clone(),
        })?;
        return Ok(model);
    }
    let default = existing
        .filter(|e| e.provider == provider)
        .map(|e| e.model.clone())
        .unwrap_or_else(|| default_embedding_model_for_provider(provider).to_string());
    eprint!("Embedding model [{}]: ", default);
    io::stderr().flush()?;
    let line = io::stdin().lock().lines().next();
    let input = match line {
        Some(Ok(s)) => s.trim().to_string(),
        _ => String::new(),
    };
    if input.is_empty() {
        Ok(default)
    } else {
        validate_embedding_config(&EmbeddingConfig {
            provider,
            model: input.clone(),
        })?;
        Ok(input)
    }
}

/// Emit a stderr warning if the env var that the embeddings client will read
/// at call time isn't set yet. Used by `--non-interactive` flows so the user
/// is told (without being prompted) that they still need to export the key.
fn warn_if_api_key_missing(provider: EmbeddingProvider) {
    let var = env_var_for_provider(provider);
    let present = std::env::var(var)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some();
    if !present {
        eprintln!(
            "warning: env var `{}` is not set; `kcl ask` will skip embeddings until you export it",
            var
        );
    }
}

/// Print the exact `export <VAR>=...` line a user will need so they don't
/// have to remember which env var maps to which provider. Always to stderr
/// (matches the rest of init's UX). Never reads or stores the key.
fn print_export_hint(cfg: &EmbeddingConfig) {
    let var = env_var_for_provider(cfg.provider);
    eprintln!();
    eprintln!(
        "Embeddings configured: provider `{}`, model `{}`.",
        cfg.provider, cfg.model
    );
    eprintln!("Set the API key in your shell (kcl never stores it):");
    match cfg.provider {
        EmbeddingProvider::Openai => {
            eprintln!("  export {var}=\"sk-...\"        # https://platform.openai.com/api-keys");
        }
        EmbeddingProvider::Openrouter => {
            eprintln!("  export {var}=\"sk-or-...\"     # https://openrouter.ai/keys");
        }
    }
    warn_if_api_key_missing(cfg.provider);
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

pub fn run(args: InitArgs<'_>) -> Result<()> {
    let InitArgs {
        non_interactive,
        enable_embeddings,
        embedding_provider,
        embedding_model,
    } = args;

    // Load the existing config first (if any) so its values can be proposed
    // as defaults during the prompts.
    let config_path = crate::paths::config_file()?;
    let config_existed = config_path.exists();
    let existing_config = if config_existed {
        Some(Config::load(&config_path)?)
    } else {
        None
    };

    // 1. Detect harnesses
    let detected = detect_harnesses();

    // 2. Select default harness (existing default biases the prompt)
    let default_harness = select_default_harness(
        &detected,
        non_interactive,
        existing_config.as_ref().map(|c| c.default_harness.as_str()),
    )?;

    // 3. Get clone directory (existing value biases the prompt)
    let clone_dir = get_clone_dir(
        non_interactive,
        existing_config.as_ref().map(|c| c.clone_dir.as_str()),
    )?;

    // 4. Build harness map
    let harness_map = build_harness_map(&detected);

    // 4b. Resolve embedding configuration (interactive prompt or non-interactive
    //     flags). May print an env-var export hint to stderr; never stores the key.
    let embedding_choice = resolve_embedding_choice(
        non_interactive,
        enable_embeddings,
        embedding_provider,
        embedding_model,
        existing_config.as_ref().and_then(|c| c.embeddings.clone()),
    )?;

    // 5. Apply choices to config — preserving prior harness customizations.
    let mut config = if let Some(existing) = existing_config {
        let new_harness_names: Vec<String> = harness_map
            .keys()
            .filter(|k| !existing.harnesses.contains_key(*k))
            .cloned()
            .collect();
        let mut config = existing;
        merge_config(&mut config, harness_map, &clone_dir, &default_harness);
        if !new_harness_names.is_empty() {
            eprintln!("Merged new harnesses: {}", new_harness_names.join(", "));
        }
        config
    } else {
        Config {
            clone_dir,
            default_harness,
            harnesses: harness_map,
            ..Config::default()
        }
    };

    match embedding_choice {
        EmbeddingChoice::Preserve => {}
        EmbeddingChoice::Set(emb) => {
            config.embeddings = Some(emb);
        }
        EmbeddingChoice::Disable => {
            config.embeddings = None;
        }
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
        let result = select_default_harness(&detected, true, None).unwrap();
        assert_eq!(result, "claude");
    }

    #[test]
    fn select_default_none_detected_uses_existing() {
        let detected: Vec<String> = vec![];
        let result = select_default_harness(&detected, true, Some("opencode")).unwrap();
        assert_eq!(result, "opencode");
    }

    #[test]
    fn select_default_one_detected() {
        let detected = vec!["opencode".to_string()];
        let result = select_default_harness(&detected, true, None).unwrap();
        assert_eq!(result, "opencode");
    }

    #[test]
    fn select_default_multiple_non_interactive() {
        let detected = vec!["claude".to_string(), "copilot".to_string()];
        let result = select_default_harness(&detected, true, None).unwrap();
        assert_eq!(result, "claude");
    }

    #[test]
    fn select_default_multiple_prefers_existing_when_detected() {
        let detected = vec!["claude".to_string(), "copilot".to_string()];
        let result = select_default_harness(&detected, true, Some("copilot")).unwrap();
        assert_eq!(result, "copilot");
    }

    #[test]
    fn select_default_multiple_falls_back_when_existing_not_detected() {
        let detected = vec!["claude".to_string(), "copilot".to_string()];
        let result = select_default_harness(&detected, true, Some("nonexistent")).unwrap();
        assert_eq!(result, "claude");
    }

    #[test]
    fn get_clone_dir_non_interactive_uses_existing() {
        let result = get_clone_dir(true, Some("/my/custom/dir")).unwrap();
        assert_eq!(result, "/my/custom/dir");
    }

    #[test]
    fn get_clone_dir_non_interactive_falls_back_to_default() {
        let result = get_clone_dir(true, None).unwrap();
        assert_eq!(result, "~/src/kcl-packages");
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
    fn resolve_embedding_choice_preserves_existing_non_interactive_without_flags() {
        let choice = resolve_embedding_choice(
            true,
            false,
            None,
            None,
            Some(EmbeddingConfig {
                provider: EmbeddingProvider::Openai,
                model: "text-embedding-3-small".to_string(),
            }),
        )
        .unwrap();

        assert!(matches!(choice, EmbeddingChoice::Preserve));
    }

    #[test]
    fn resolve_embedding_choice_provider_flag_implies_enable_non_interactive() {
        let choice = resolve_embedding_choice(true, false, Some("openrouter"), None, None)
            .unwrap();

        match choice {
            EmbeddingChoice::Set(cfg) => {
                assert_eq!(cfg.provider, EmbeddingProvider::Openrouter);
                assert_eq!(cfg.model, "openai/text-embedding-3-small");
            }
            _ => panic!("expected embeddings to be enabled from provider flag"),
        }
    }

    #[test]
    fn resolve_embedding_choice_provider_change_uses_new_provider_default_model() {
        let choice = resolve_embedding_choice(
            true,
            true,
            Some("openrouter"),
            None,
            Some(EmbeddingConfig {
                provider: EmbeddingProvider::Openai,
                model: "text-embedding-3-small".to_string(),
            }),
        )
        .unwrap();

        match choice {
            EmbeddingChoice::Set(cfg) => {
                assert_eq!(cfg.provider, EmbeddingProvider::Openrouter);
                assert_eq!(cfg.model, "openai/text-embedding-3-small");
            }
            _ => panic!("expected updated embeddings config"),
        }
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
                        prepared_args: vec![],
                    },
                );
                m
            },
            embeddings: None,
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
                prepared_args: vec![],
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
                prepared_args: vec![],
            },
        );

        merge_config(&mut existing, new_harnesses, "~/new-dir", "opencode");

        // Existing claude entry must be preserved (user's customization).
        assert_eq!(existing.harnesses["claude"].command, "claude");
        // New opencode template should be added.
        assert!(existing.harnesses.contains_key("opencode"));
        assert_eq!(existing.harnesses["opencode"].command, "opencode");
        // clone_dir reflects the user's choice from the prompt.
        assert_eq!(existing.clone_dir, "~/new-dir");
        // default_harness reflects the user's choice from the prompt.
        assert_eq!(existing.default_harness, "opencode");
    }

    #[test]
    fn merge_config_applies_chosen_default_harness() {
        let mut existing = Config {
            clone_dir: "~/src/kcl-packages".to_string(),
            default_harness: "nonexistent".to_string(),
            default_timeout: 120,
            harnesses: HashMap::new(),
            embeddings: None,
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
                prepared_args: vec![],
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
        let default_harness = select_default_harness(&detected, true, None).unwrap();
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
                prepared_args: vec![],
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
                prepared_args: vec![],
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
                prepared_args: vec![],
            },
        );

        // Simulate the second init accepting the proposed defaults from the
        // prompt — i.e. the existing clone_dir flows back through.
        merge_config(&mut loaded, new_harnesses, "/my/custom/dir", "claude");
        loaded.save(&config_path).unwrap();

        // Verify merge behavior
        let final_config = Config::load(&config_path).unwrap();
        // Existing claude harness customization should NOT be overwritten.
        assert_eq!(final_config.harnesses["claude"].command, "my-custom-claude");
        // New opencode should be added.
        assert!(final_config.harnesses.contains_key("opencode"));
        // clone_dir is preserved because the user accepted the proposed default.
        assert_eq!(final_config.clone_dir, "/my/custom/dir");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
