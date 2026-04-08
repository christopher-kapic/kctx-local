use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::{HarnessConfig, PromptMode};

/// The result of running a harness subprocess.
#[derive(Debug)]
pub struct HarnessOutput {
    /// The subprocess exit code (None if killed by signal).
    pub exit_code: Option<i32>,
    /// The full captured stdout.
    pub stdout: String,
    /// The full captured stderr.
    #[allow(dead_code)]
    pub stderr: String,
}

/// Build the prompt string sent to the harness.
///
/// Template from the design spec:
/// ```text
/// You are answering a question about the {display_name} ({identifier}) codebase.
/// The codebase is in your current working directory.
///
/// [optional context block]
///
/// Question: {question}
///
/// Explore the codebase and answer precisely. Reference file paths.
/// ```
pub fn build_prompt(
    display_name: &str,
    identifier: &str,
    question: &str,
    context: Option<&[String]>,
) -> String {
    let mut prompt = format!(
        "You are answering a question about the {} ({}) codebase.\n\
         The codebase is in your current working directory.\n",
        display_name, identifier
    );

    if let Some(recent_questions) = context
        && !recent_questions.is_empty()
    {
        prompt.push_str(
            "\nRecent questions asked about this package (for context, avoid re-exploring these topics):\n",
        );
        for q in recent_questions {
            prompt.push_str(&format!("- {}\n", q));
        }
        prompt.push('\n');
    }

    prompt.push_str(&format!(
        "\nQuestion: {}\n\n\
         Explore the codebase and answer precisely. Reference file paths.\n",
        question
    ));

    prompt
}

/// Build the argument list for the harness, replacing `{prompt}` placeholders
/// when using `prompt_mode: arg`.
pub fn build_args(harness: &HarnessConfig, prompt: &str) -> Vec<String> {
    match harness.prompt_mode {
        PromptMode::Arg => harness
            .args
            .iter()
            .map(|arg| arg.replace("{prompt}", prompt))
            .collect(),
        PromptMode::Stdin => harness.args.clone(),
    }
}

/// Spawn a harness subprocess, stream its output in real-time, and return
/// the captured output and exit code.
///
/// - `cwd`: the package path (working directory for the subprocess)
/// - `timeout_secs`: kill the subprocess after this many seconds
/// - `stream_stdout`: if true, stream stdout lines to the caller's stdout in real-time
pub async fn run_harness(
    harness: &HarnessConfig,
    prompt: &str,
    cwd: &Path,
    timeout_secs: u64,
    stream_stdout: bool,
) -> Result<HarnessOutput> {
    let args = build_args(harness, prompt);

    let stdin_cfg = match harness.prompt_mode {
        PromptMode::Stdin => Stdio::piped(),
        PromptMode::Arg => Stdio::null(),
    };

    let mut child = Command::new(&harness.command)
        .args(&args)
        .current_dir(cwd)
        .stdin(stdin_cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning harness command: {}", harness.command))?;

    // Write prompt to stdin if using stdin mode
    if harness.prompt_mode == PromptMode::Stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("writing prompt to harness stdin")?;
        // Drop stdin to close it, signaling EOF
    }

    let stdout_pipe = child.stdout.take().expect("stdout should be piped");
    let stderr_pipe = child.stderr.take().expect("stderr should be piped");

    let mut stdout_reader = BufReader::new(stdout_pipe).lines();
    let mut stderr_reader = BufReader::new(stderr_pipe).lines();

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    let timeout = tokio::time::sleep(Duration::from_secs(timeout_secs));
    tokio::pin!(timeout);

    // Read stdout/stderr concurrently, with timeout
    loop {
        tokio::select! {
            line = stdout_reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if stream_stdout {
                            println!("{}", line);
                        }
                        if !stdout_buf.is_empty() {
                            stdout_buf.push('\n');
                        }
                        stdout_buf.push_str(&line);
                    }
                    Ok(None) => {
                        // stdout closed — drain remaining stderr then wait for exit
                        while let Ok(Some(line)) = stderr_reader.next_line().await {
                            if !stderr_buf.is_empty() {
                                stderr_buf.push('\n');
                            }
                            stderr_buf.push_str(&line);
                        }
                        break;
                    }
                    Err(e) => {
                        bail!("reading harness stdout: {}", e);
                    }
                }
            }
            line = stderr_reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if !stderr_buf.is_empty() {
                            stderr_buf.push('\n');
                        }
                        stderr_buf.push_str(&line);
                    }
                    Ok(None) => {
                        // stderr closed — drain remaining stdout then wait for exit
                        while let Ok(Some(line)) = stdout_reader.next_line().await {
                            if stream_stdout {
                                println!("{}", line);
                            }
                            if !stdout_buf.is_empty() {
                                stdout_buf.push('\n');
                            }
                            stdout_buf.push_str(&line);
                        }
                        break;
                    }
                    Err(e) => {
                        bail!("reading harness stderr: {}", e);
                    }
                }
            }
            _ = &mut timeout => {
                // Kill the subprocess on timeout
                let _ = child.kill().await;
                bail!(
                    "harness timed out after {} seconds",
                    timeout_secs
                );
            }
        }
    }

    let status = child.wait().await.context("waiting for harness to exit")?;

    Ok(HarnessOutput {
        exit_code: status.code(),
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HarnessConfig, PromptMode};

    #[test]
    fn prompt_builder_basic() {
        let prompt = build_prompt("Axum", "axum", "How does routing work?", None);

        assert!(prompt.contains("Axum (axum)"));
        assert!(prompt.contains("Question: How does routing work?"));
        assert!(prompt.contains("Explore the codebase and answer precisely"));
        assert!(prompt.contains("current working directory"));
        // Should NOT contain context block
        assert!(!prompt.contains("Recent questions"));
    }

    #[test]
    fn prompt_builder_with_context() {
        let recent = vec![
            "How do middleware work?".to_string(),
            "What extractors are available?".to_string(),
        ];
        let prompt = build_prompt("Axum", "axum", "How does routing work?", Some(&recent));

        assert!(prompt.contains("Axum (axum)"));
        assert!(prompt.contains("Question: How does routing work?"));
        assert!(prompt.contains("Recent questions asked about this package"));
        assert!(prompt.contains("- How do middleware work?"));
        assert!(prompt.contains("- What extractors are available?"));
    }

    #[test]
    fn prompt_builder_with_empty_context() {
        let recent: Vec<String> = vec![];
        let prompt = build_prompt("Axum", "axum", "How does routing work?", Some(&recent));

        // Empty context list should not produce the context block
        assert!(!prompt.contains("Recent questions"));
    }

    #[test]
    fn build_args_replaces_prompt_placeholder() {
        let harness = HarnessConfig {
            command: "claude".to_string(),
            args: vec![
                "-p".to_string(),
                "{prompt}".to_string(),
                "--bare".to_string(),
            ],
            prompt_mode: PromptMode::Arg,
        };

        let args = build_args(&harness, "test prompt");
        assert_eq!(args, vec!["-p", "test prompt", "--bare"]);
    }

    #[test]
    fn build_args_replaces_multiple_placeholders() {
        let harness = HarnessConfig {
            command: "agent".to_string(),
            args: vec![
                "--query={prompt}".to_string(),
                "--title={prompt}".to_string(),
            ],
            prompt_mode: PromptMode::Arg,
        };

        let args = build_args(&harness, "my question");
        assert_eq!(args, vec!["--query=my question", "--title=my question"]);
    }

    #[test]
    fn build_args_stdin_mode_no_replacement() {
        let harness = HarnessConfig {
            command: "my-agent".to_string(),
            args: vec!["--query".to_string()],
            prompt_mode: PromptMode::Stdin,
        };

        let args = build_args(&harness, "test prompt");
        assert_eq!(args, vec!["--query"]);
        // {prompt} should NOT appear in args for stdin mode
        assert!(!args.iter().any(|a| a.contains("{prompt}")));
    }

    #[tokio::test]
    async fn run_harness_captures_stdout_and_exit_code() {
        let harness = HarnessConfig {
            command: "echo".to_string(),
            args: vec!["hello from harness".to_string()],
            prompt_mode: PromptMode::Arg,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "", &cwd, 10, false).await.unwrap();

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "hello from harness");
    }

    #[tokio::test]
    async fn run_harness_returns_nonzero_exit_code() {
        let harness = HarnessConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "exit 42".to_string()],
            prompt_mode: PromptMode::Arg,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "", &cwd, 10, false).await.unwrap();

        assert_eq!(result.exit_code, Some(42));
    }

    #[tokio::test]
    async fn run_harness_captures_stderr() {
        let harness = HarnessConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo out; echo err >&2".to_string()],
            prompt_mode: PromptMode::Arg,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "", &cwd, 10, false).await.unwrap();

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "out");
        assert_eq!(result.stderr.trim(), "err");
    }

    #[tokio::test]
    async fn run_harness_stdin_mode_sends_prompt() {
        let harness = HarnessConfig {
            command: "cat".to_string(),
            args: vec![],
            prompt_mode: PromptMode::Stdin,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "hello via stdin", &cwd, 10, false)
            .await
            .unwrap();

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "hello via stdin");
    }

    #[tokio::test]
    async fn run_harness_arg_mode_replaces_prompt() {
        let harness = HarnessConfig {
            command: "echo".to_string(),
            args: vec!["{prompt}".to_string()],
            prompt_mode: PromptMode::Arg,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "the prompt text", &cwd, 10, false)
            .await
            .unwrap();

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "the prompt text");
    }

    #[tokio::test]
    async fn run_harness_timeout() {
        let harness = HarnessConfig {
            command: "sleep".to_string(),
            args: vec!["60".to_string()],
            prompt_mode: PromptMode::Arg,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "", &cwd, 1, false).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }
}
