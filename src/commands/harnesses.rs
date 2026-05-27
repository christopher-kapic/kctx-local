use anyhow::Result;
use serde::Serialize;

use crate::cli::HarnessesCommand;
use crate::config::{Config, HarnessConfig, PromptMode};

pub fn run(command: &HarnessesCommand) -> Result<()> {
    match command {
        HarnessesCommand::List { verbose, json } => cmd_list(*verbose, *json),
    }
}

/// One harness row in the listing. Mirrors the on-disk fields plus a
/// `is_default` flag so callers can see which one `kcl ask` will pick by
/// default.
#[derive(Debug, Serialize)]
struct HarnessRow<'a> {
    name: &'a str,
    is_default: bool,
    command: &'a str,
    prompt_mode: &'static str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    model_args: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<&'a str>,
}

fn prompt_mode_str(mode: &PromptMode) -> &'static str {
    match mode {
        PromptMode::Arg => "arg",
        PromptMode::Stdin => "stdin",
    }
}

fn build_rows<'a>(config: &'a Config) -> Vec<HarnessRow<'a>> {
    let mut names: Vec<&String> = config.harnesses.keys().collect();
    names.sort();

    names
        .into_iter()
        .map(|name| {
            let h: &HarnessConfig = &config.harnesses[name];
            HarnessRow {
                name: name.as_str(),
                is_default: name == &config.default_harness,
                command: h.command.as_str(),
                prompt_mode: prompt_mode_str(&h.prompt_mode),
                model_args: h.model_args.as_slice(),
                default_model: h.default_model.as_deref(),
            }
        })
        .collect()
}

fn cmd_list(verbose: bool, json: bool) -> Result<()> {
    let config = Config::load_or_default()?;
    let rows = build_rows(&config);

    if json {
        let out = serde_json::to_string_pretty(&rows)?;
        println!("{out}");
        return Ok(());
    }

    if rows.is_empty() {
        eprintln!("No harnesses configured. Run `kcl init` to detect installed harnesses.");
        return Ok(());
    }

    if !verbose {
        for row in &rows {
            if row.is_default {
                println!("{} (default)", row.name);
            } else {
                println!("{}", row.name);
            }
        }
        return Ok(());
    }

    // Verbose: tab-separated columns. Keep stable so agents can parse.
    // name<TAB>command<TAB>prompt_mode<TAB>default_model<TAB>model_args
    for row in &rows {
        let default_model = row.default_model.unwrap_or("-");
        let model_args = if row.model_args.is_empty() {
            "-".to_string()
        } else {
            row.model_args.join(" ")
        };
        let marker = if row.is_default { " *" } else { "" };
        println!(
            "{}{}\t{}\t{}\t{}\t{}",
            row.name, marker, row.command, row.prompt_mode, default_model, model_args,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HarnessConfig;
    use std::collections::HashMap;

    fn sample_config() -> Config {
        let mut harnesses = HashMap::new();
        harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec!["-p".to_string(), "{prompt}".to_string()],
                prompt_mode: PromptMode::Arg,
                model_args: vec!["--model".to_string(), "{model}".to_string()],
                default_model: Some("claude-sonnet-4-6".to_string()),
                prepared_args: vec![],
                inject_explore_toolkit: true,
            },
        );
        harnesses.insert(
            "pi".to_string(),
            HarnessConfig {
                command: "pi".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Stdin,
                model_args: vec![],
                default_model: None,
                prepared_args: vec![],
                inject_explore_toolkit: true,
            },
        );
        Config {
            clone_dir: "~/src/kcl-packages".to_string(),
            default_harness: "claude".to_string(),
            default_timeout: 120,
            harnesses,
        }
    }

    #[test]
    fn build_rows_sorts_alphabetically_and_marks_default() {
        let config = sample_config();
        let rows = build_rows(&config);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "claude");
        assert!(rows[0].is_default);
        assert_eq!(rows[0].default_model, Some("claude-sonnet-4-6"));
        assert_eq!(rows[1].name, "pi");
        assert!(!rows[1].is_default);
        assert_eq!(rows[1].default_model, None);
    }

    #[test]
    fn rows_serialize_to_json_with_omitted_empty_fields() {
        let config = sample_config();
        let rows = build_rows(&config);
        let json = serde_json::to_string(&rows).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 2);

        // claude has both fields populated
        assert_eq!(arr[0]["name"], "claude");
        assert_eq!(arr[0]["is_default"], true);
        assert_eq!(arr[0]["default_model"], "claude-sonnet-4-6");
        assert!(arr[0]["model_args"].is_array());

        // pi omits default_model and model_args (both empty/none)
        assert_eq!(arr[1]["name"], "pi");
        assert!(arr[1].get("default_model").is_none());
        assert!(arr[1].get("model_args").is_none());
    }
}
