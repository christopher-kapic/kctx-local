use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::{HarnessConfig, PromptMode};
use crate::models::prepared_context::PreparedContext;

/// The result of running a harness subprocess.
#[derive(Debug)]
pub struct HarnessOutput {
    /// The subprocess exit code (None if killed by signal).
    pub exit_code: Option<i32>,
    /// The full captured stdout.
    pub stdout: String,
    /// The full captured stderr.
    #[cfg_attr(not(test), allow(dead_code))]
    pub stderr: String,
}

/// How much a prepared orientation map can be trusted, derived from how far
/// its recorded commit is behind the working tree.
///
/// A single classification drives BOTH the directive line inside the map block
/// and the closing instruction, so the two can never contradict — a stale or
/// unknown-age map must not be announced as "authoritative for this exact
/// codebase" while the closing text simultaneously says it may be stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MapFreshness {
    /// Recorded commit == current HEAD: trust the map fully.
    Fresh,
    /// A few commits behind: structure still reliable, expect minor drift.
    SlightlyStale,
    /// Staleness unknown, or far enough behind to only be a rough guide.
    Unknown,
}

/// Maps at most this many commits behind are still treated as a reliable
/// structural guide ([`MapFreshness::SlightlyStale`]); beyond this, or when
/// staleness is unknown, the map is downgraded to [`MapFreshness::Unknown`].
const SLIGHTLY_STALE_MAX_COMMITS: usize = 10;

impl MapFreshness {
    fn classify(commits_behind: Option<usize>) -> Self {
        match commits_behind {
            Some(0) => Self::Fresh,
            Some(n) if n <= SLIGHTLY_STALE_MAX_COMMITS => Self::SlightlyStale,
            _ => Self::Unknown,
        }
    }
}

/// Inline summary of the `kcl explore` toolkit, appended to the harness prompt
/// when `inject_toolkit` is true. Kept compact because it goes into every
/// `kcl ask` invocation, but grouped by intent so the agent reaches for the
/// right primitive instead of falling back to raw `grep`/`cat`/`find`.
///
/// Every command operates on the package in the current working directory by
/// default. Common flags on every command: `--json` (structured output),
/// `--max-bytes N` (cap response size), `--help` (full flag list).
const EXPLORE_TOOLKIT_BLOCK: &str = concat!(
    "\n--- BEGIN EXPLORATION TOOLKIT ---\n",
    "The `kcl explore` CLI is available on this machine and operates on the package in the current working directory. It's index-backed and returns structured, precise answers far more cheaply than full-file reads — but it only helps for code questions, so don't reach for it indiscriminately.\n",
    "\n",
    "Reach for `kcl explore` when the question is about code: locating a definition, finding callers/usages, mapping structure, assessing change impact, or navigating an unfamiliar codebase.\n",
    "\n",
    "Stick with your built-in `Read`/`Grep`/`Glob` tools when: reading docs / READMEs / examples / config; opening a file whose path you already know; searching by filename or extension; or working in a small repo where indexing overhead isn't worth it. If a `kcl explore` call is taking noticeably long, fall back to `Grep`/`Read` rather than waiting.\n",
    "\n",
    "Common flags on every command: `--json` for structured output, `--max-bytes N` to cap response size, `--help` for full details.\n",
    "\n",
    "Orientation:\n",
    "- `kcl explore tree [path]` — annotated directory listing with per-file symbol counts (no content)\n",
    "- `kcl explore outline <file>` — symbols, parents, and imports for one file (use this instead of reading whole files to discover structure)\n",
    "- `kcl explore hot [--limit N]` — most-recently-modified files\n",
    "\n",
    "Definition / reference lookup:\n",
    "- `kcl explore symbol <name> [--prefix] [--kind K]` — definition sites for a symbol (kinds: function, method, struct, enum, trait, class, interface, type, const, module)\n",
    "- `kcl explore word <token> [-i]` — exact-identifier inverted index lookup (every line containing this token)\n",
    "- `kcl explore search <pattern> [--type T] [--glob G] [--context N] [-i]` — regex content search via ripgrep, budget-capped\n",
    "\n",
    "Targeted reading:\n",
    "- `kcl explore read <file> [--start N] [--end M]` — read a line range with a content hash header (use after `outline` to pull just the function body you need)\n",
    "\n",
    "Blast-radius / structure analysis:\n",
    "- `kcl explore deps <file> [--direction forward|reverse|both] [--hops N]` — file-level import graph\n",
    "- `kcl explore impact <symbol> [--hops N] [--file PATH]` — symbol-level callsites across the codebase; `--file` scopes callers when a name is ambiguous\n",
    "- `kcl explore circular` — detect import cycles\n",
    "\n",
    "Suggested flow: start with `outline`/`tree` to orient, use `symbol`/`word`/`search` to locate, then `read` line ranges for the parts you actually need. Reach for `deps`/`impact` when assessing change risk.\n",
    "--- END EXPLORATION TOOLKIT ---\n",
);

/// Build the prompt string sent to the harness.
///
/// The prepared-map block is injected near the top so the agent sees the
/// high-signal orientation hints first. Delimiters are chosen to be obvious
/// to both humans and LLM agents.
///
/// When `inject_toolkit` is true, an inline summary of the `kcl explore`
/// toolbox is appended after the prepared map (if any) and before the closing
/// instruction so the harness sees the navigation primitives it has access to.
///
/// When `shallow` is true (the on-disk clone is a shallow/depth-truncated
/// clone), a note is injected near the top of the prompt granting the harness
/// permission to deepen the history itself if the question requires more than
/// the current branch tip.
#[allow(clippy::too_many_arguments)]
pub fn build_prompt(
    display_name: &str,
    identifier: &str,
    question: &str,
    context: Option<&[String]>,
    prepared: Option<&PreparedContext>,
    current_commit_sha: Option<&str>,
    commits_behind: Option<usize>,
    inject_toolkit: bool,
    shallow: bool,
) -> String {
    let mut prompt = format!(
        concat!(
            "You are answering a question about the {} ({}) codebase.\n",
            "The codebase is in your current working directory.\n",
        ),
        display_name, identifier
    );

    // Shallow-clone note: warn the harness that history is truncated and grant
    // it permission to deepen the clone itself if it needs more than the tip.
    if shallow {
        prompt.push_str(
            "\nNote: this repository is a shallow clone — its history is truncated (only the tip commit of each branch is present). If answering requires git history, blame, or a version other than the current branch tip, you have permission to deepen it yourself by running `git fetch --unshallow` (or `git fetch --deepen=N`) inside the repository before proceeding.\n",
        );
    }

    // Single freshness classification shared by the in-map directive and the
    // closing instruction so they cannot contradict. `None` ⇔ no prepared map.
    let freshness = prepared.map(|_| MapFreshness::classify(commits_behind));

    // 1. Prepared orientation map block (if present).
    if let Some(p) = prepared {
        let ts = p.created_at.to_rfc3339();
        let rec = p.git_commit_sha.as_deref().unwrap_or("unknown");
        let cur = current_commit_sha.unwrap_or("unknown");
        let behind = commits_behind
            .map(|n| {
                if n == 0 {
                    String::new()
                } else {
                    format!(", {} commits behind", n)
                }
            })
            .unwrap_or_default();
        let scope = &p.prepare_scope_at_time;

        let header = format!(
            "Prepared at {} on commit `{}` (current HEAD `{}`{}). Scope: `{}`",
            ts, rec, cur, behind, scope
        );

        prompt.push_str("\n--- BEGIN PREPARED ORIENTATION MAP ---\n");
        // Directive strength must match `freshness`: only a fresh map may be
        // called authoritative for the exact current codebase. (Inside this
        // branch `freshness` is always `Some` since `prepared` is `Some`.)
        let directive = match freshness {
            Some(MapFreshness::Fresh) => {
                "This is an accurate, authoritative orientation for this exact codebase. Rely on it instead of re-deriving structure.\n"
            }
            Some(MapFreshness::SlightlyStale) => {
                "This orientation map was accurate a few commits ago and is still a reliable guide to where things live; rely on it but expect minor drift in recently changed areas.\n"
            }
            _ => {
                "This orientation map may be out of date. Use it as a guide to where things live, not as ground truth — verify specifics before relying on them.\n"
            }
        };
        prompt.push_str(directive);
        prompt.push_str(&header);
        prompt.push('\n');
        prompt.push_str(&p.content);
        if !p.content.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("--- END PREPARED ORIENTATION MAP ---\n");
    }

    // 2. Existing recent-questions context (unchanged behavior).
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

    // 3. Optional exploration toolkit summary. Placed after the prepared map
    //    (and any recent-questions block) but before the closing instruction
    //    so the agent sees the navigation primitives it has available.
    if inject_toolkit {
        prompt.push_str(EXPLORE_TOOLKIT_BLOCK);
    }

    // Closing instruction. When no prepared map is present, keep the original
    // explore-everything wording. When a map IS present, make the instruction
    // freshness-aware so the agent trusts the map (and skips the broad tree
    // scan) to whatever degree the staleness signal allows. Unknown or large
    // staleness falls back to the cautious explore wording so correctness is
    // never sacrificed.
    let closing = match freshness {
        // No prepared map: keep the original explore-everything wording.
        None => "Explore the codebase and answer precisely. Reference file paths.",
        // Fresh: map matches current HEAD. Trust it fully.
        Some(MapFreshness::Fresh) => {
            "Treat the prepared orientation map above as the authoritative primary source. Do NOT perform a broad tree scan: answer directly from the map, opening only files it points to or that are strictly necessary to answer. Reference file paths."
        }
        // Slightly stale: structure is still reliable; only verify the
        // handful of files plausibly touched by recent changes.
        Some(MapFreshness::SlightlyStale) => {
            "Treat the prepared orientation map above as the authoritative primary source. Do NOT perform a broad tree scan: trust the map's structure, but it is a few commits behind — additionally verify only the specific files plausibly affected by recent changes. Open only files the map points to or that are strictly necessary to answer. Reference file paths."
        }
        // Unknown or very stale: cautious fallback close to the original
        // explore behavior, but still let the map guide where to look.
        Some(MapFreshness::Unknown) => {
            "The prepared orientation map above may be stale. Use it as a starting guide, but explore the codebase to verify and answer precisely. Reference file paths."
        }
    };

    prompt.push_str(&format!("\nQuestion: {}\n\n{}\n", question, closing));

    prompt
}

/// Build the argument list for the harness, replacing `{prompt}` placeholders
/// when using `prompt_mode: arg` and appending `model_args` (with `{model}`
/// replaced) when a model was requested and the harness supports it.
///
/// If `model` is `Some` but `harness.model_args` is empty, the model is
/// silently ignored — the harness simply doesn't support model selection.
///
/// When `map_present` is true (a prepared orientation map was injected into
/// this `kcl ask`), the harness's `prepared_args` are appended last so the
/// config can impose a mechanical exploration ceiling (e.g. `--max-turns`,
/// a restricted `--allowedTools`) even if the model ignores the prompt's
/// "trust the map" guidance. The same placeholder substitution as the other
/// arg lists is applied: `{prompt}` is replaced when `prompt_mode` is `arg`,
/// and `{model}` is replaced when a model is resolved. When `map_present` is
/// false the produced argv is byte-for-byte identical to the previous
/// behavior — `prepared_args` is never consulted (so `kcl prepare`, which
/// passes `false`, and a plain map-less `ask` are unaffected).
pub fn build_args(
    harness: &HarnessConfig,
    prompt: &str,
    model: Option<&str>,
    map_present: bool,
) -> Vec<String> {
    let mut args: Vec<String> = match harness.prompt_mode {
        PromptMode::Arg => harness
            .args
            .iter()
            .map(|arg| arg.replace("{prompt}", prompt))
            .collect(),
        PromptMode::Stdin => harness.args.clone(),
    };

    if let Some(model) = model
        && !harness.model_args.is_empty()
    {
        for arg in &harness.model_args {
            args.push(arg.replace("{model}", model));
        }
    }

    if map_present && !harness.prepared_args.is_empty() {
        for arg in &harness.prepared_args {
            // Mirror the substitution applied to `args` / `model_args` so
            // combined-form values keep working. `{prompt}` is only meaningful
            // in `arg` prompt mode (matching how `args` is handled above);
            // `{model}` is substituted only when a model was resolved.
            let mut a = match harness.prompt_mode {
                PromptMode::Arg => arg.replace("{prompt}", prompt),
                PromptMode::Stdin => arg.clone(),
            };
            if let Some(model) = model {
                a = a.replace("{model}", model);
            }
            args.push(a);
        }
    }

    args
}

/// Wait for SIGINT or SIGTERM. On non-Unix platforms, returns a future that
/// never resolves (signals are handled by the OS default behavior).
#[cfg(unix)]
async fn setup_signal_handler() {
    use tokio::signal::unix::{SignalKind, signal};
    let sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("warning: failed to register SIGINT handler: {}", e);
            None
        }
    };
    let sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("warning: failed to register SIGTERM handler: {}", e);
            None
        }
    };
    match (sigint, sigterm) {
        (Some(mut sigint), Some(mut sigterm)) => {
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
        }
        (Some(mut sigint), None) => {
            sigint.recv().await;
        }
        (None, Some(mut sigterm)) => {
            sigterm.recv().await;
        }
        (None, None) => {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn setup_signal_handler() {
    std::future::pending::<()>().await;
}

/// Kill a harness subprocess and all processes in its group, then reap.
///
/// On Unix, sends SIGKILL to the entire process group (the child was spawned
/// with `process_group(0)` so it leads its own group). Falls back to killing
/// just the child on non-Unix or if the group kill fails.
async fn kill_and_reap(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // Safety: sending a signal to a process group is a well-defined POSIX operation.
            let ret = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            if ret == -1 {
                let err = std::io::Error::last_os_error();
                // ESRCH means the process group already exited — not worth reporting.
                if err.raw_os_error() != Some(libc::ESRCH) {
                    eprintln!(
                        "warning: failed to kill harness process group (pid {}): {}",
                        pid, err
                    );
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(e) = child.kill().await {
            eprintln!("warning: failed to kill harness process: {}", e);
        }
    }

    let _ = child.wait().await;
}

/// Spawn a harness subprocess, stream its output in real-time, and return
/// the captured output and exit code.
///
/// - `cwd`: the package path (working directory for the subprocess)
/// - `timeout_secs`: kill the subprocess after this many seconds
/// - `stream_stdout`: if true, stream stdout lines to the caller's stdout in real-time
/// - `model`: optional model identifier to forward via the harness's
///   `model_args` template. Silently ignored if the harness has no template.
///
/// This is the map-unaware entry point (no `prepared_args` ever appended).
/// `kcl prepare` and any other caller without an injected orientation map use
/// this; `kcl ask` calls [`run_harness_with_prepared_args`] instead so it can
/// opt into the mechanical exploration ceiling when a map is present.
pub async fn run_harness(
    harness: &HarnessConfig,
    prompt: &str,
    cwd: &Path,
    timeout_secs: u64,
    stream_stdout: bool,
    model: Option<&str>,
) -> Result<HarnessOutput> {
    run_harness_with_prepared_args(
        harness,
        prompt,
        cwd,
        timeout_secs,
        stream_stdout,
        model,
        false,
    )
    .await
}

/// Spawn a harness subprocess, optionally appending the harness's
/// `prepared_args` (mechanical exploration ceiling) when `map_present` is true.
///
/// Identical to [`run_harness`] in every other respect. `map_present` is
/// threaded straight through to [`build_args`]; when it is false the produced
/// argv (and therefore behavior) is byte-for-byte identical to `run_harness`.
/// Only `kcl ask` with an injected prepared orientation map passes `true`.
pub async fn run_harness_with_prepared_args(
    harness: &HarnessConfig,
    prompt: &str,
    cwd: &Path,
    timeout_secs: u64,
    stream_stdout: bool,
    model: Option<&str>,
    map_present: bool,
) -> Result<HarnessOutput> {
    let args = build_args(harness, prompt, model, map_present);

    let stdin_cfg = match harness.prompt_mode {
        PromptMode::Stdin => Stdio::piped(),
        PromptMode::Arg => Stdio::null(),
    };

    let mut cmd = Command::new(&harness.command);
    cmd.args(&args)
        .current_dir(cwd)
        .stdin(stdin_cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
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
        // Explicitly drop stdin to close the pipe and signal EOF to the child;
        // without this the harness may block waiting for more input.
        drop(stdin);
    }

    let stdout_pipe = child.stdout.take().expect("stdout should be piped");
    let stderr_pipe = child.stderr.take().expect("stderr should be piped");

    let mut stdout_reader = BufReader::new(stdout_pipe).lines();
    let mut stderr_reader = BufReader::new(stderr_pipe).lines();

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    let timeout = tokio::time::sleep(Duration::from_secs(timeout_secs));
    tokio::pin!(timeout);

    let signal_fut = setup_signal_handler();
    tokio::pin!(signal_fut);

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
                        // stdout closed — drain remaining stderr with a timeout
                        // to avoid hanging if the pipe stays open indefinitely
                        let drain = async {
                            while let Ok(Some(line)) = stderr_reader.next_line().await {
                                if !stderr_buf.is_empty() {
                                    stderr_buf.push('\n');
                                }
                                stderr_buf.push_str(&line);
                            }
                        };
                        let _ = tokio::time::timeout(Duration::from_secs(5), drain).await;
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
                        // stderr closed — drain remaining stdout with a timeout
                        // to avoid hanging if the pipe stays open indefinitely
                        let drain = async {
                            while let Ok(Some(line)) = stdout_reader.next_line().await {
                                if stream_stdout {
                                    println!("{}", line);
                                }
                                if !stdout_buf.is_empty() {
                                    stdout_buf.push('\n');
                                }
                                stdout_buf.push_str(&line);
                            }
                        };
                        let _ = tokio::time::timeout(Duration::from_secs(5), drain).await;
                        break;
                    }
                    Err(e) => {
                        bail!("reading harness stderr: {}", e);
                    }
                }
            }
            _ = &mut timeout => {
                kill_and_reap(&mut child).await;
                bail!(
                    "harness timed out after {} seconds",
                    timeout_secs
                );
            }
            _ = &mut signal_fut => {
                kill_and_reap(&mut child).await;
                bail!("interrupted by signal");
            }
        }
    }

    let status = match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(res) => res.context("waiting for harness to exit")?,
        Err(_) => {
            kill_and_reap(&mut child).await;
            bail!("harness did not exit after closing output pipes");
        }
    };

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
        let prompt = build_prompt(
            "Axum",
            "axum",
            "How does routing work?",
            None,
            None,
            None,
            None,
            false,
            false,
        );

        assert!(prompt.contains("Axum (axum)"));
        assert!(prompt.contains("Question: How does routing work?"));
        assert!(prompt.contains("Explore the codebase and answer precisely"));
        assert!(prompt.contains("current working directory"));
        // Should NOT contain context block
        assert!(!prompt.contains("Recent questions"));
        // No prepared block when absent
        assert!(!prompt.contains("PREPARED ORIENTATION MAP"));
        // No toolkit when injection is off.
        assert!(!prompt.contains("BEGIN EXPLORATION TOOLKIT"));
    }

    #[test]
    fn prompt_builder_with_context() {
        let recent = vec![
            "How do middleware work?".to_string(),
            "What extractors are available?".to_string(),
        ];
        let prompt = build_prompt(
            "Axum",
            "axum",
            "How does routing work?",
            Some(&recent),
            None,
            None,
            None,
            false,
            false,
        );

        assert!(prompt.contains("Axum (axum)"));
        assert!(prompt.contains("Question: How does routing work?"));
        assert!(prompt.contains("Recent questions asked about this package"));
        assert!(prompt.contains("- How do middleware work?"));
        assert!(prompt.contains("- What extractors are available?"));
    }

    #[test]
    fn prompt_builder_with_empty_context() {
        let recent: Vec<String> = vec![];
        let prompt = build_prompt(
            "Axum",
            "axum",
            "How does routing work?",
            Some(&recent),
            None,
            None,
            None,
            false,
            false,
        );

        // Empty context list should not produce the context block
        assert!(!prompt.contains("Recent questions"));
    }

    #[test]
    fn prompt_injects_toolkit_when_flag_is_true() {
        let prompt = build_prompt(
            "Axum",
            "axum",
            "How does routing work?",
            None,
            None,
            None,
            None,
            true,
            false,
        );
        assert!(prompt.contains("--- BEGIN EXPLORATION TOOLKIT ---"));
        assert!(prompt.contains("--- END EXPLORATION TOOLKIT ---"));
        // All ten commands appear, identified by the leading backticked name.
        for c in [
            "`kcl explore tree",
            "`kcl explore outline",
            "`kcl explore symbol",
            "`kcl explore word",
            "`kcl explore search",
            "`kcl explore read",
            "`kcl explore deps",
            "`kcl explore impact",
            "`kcl explore hot",
            "`kcl explore circular",
        ] {
            assert!(prompt.contains(c), "toolkit missing `{}`", c);
        }
        // High-value flags surfaced so the agent doesn't have to discover
        // them via `--help` on every call.
        assert!(prompt.contains("--prefix"));
        assert!(prompt.contains("--kind"));
        assert!(prompt.contains("--hops"));
        assert!(prompt.contains("--direction"));
        assert!(prompt.contains("--start"));
        assert!(prompt.contains("--file"));
        // Intent grouping headers.
        assert!(prompt.contains("Orientation:"));
        assert!(prompt.contains("Definition / reference lookup:"));
        assert!(prompt.contains("Blast-radius / structure analysis:"));
        // The toolkit block sits before the closing instruction.
        let toolkit_idx = prompt
            .find("BEGIN EXPLORATION TOOLKIT")
            .expect("toolkit must be present");
        let closing_idx = prompt
            .find("Explore the codebase and answer precisely")
            .expect("closing must be present");
        assert!(
            toolkit_idx < closing_idx,
            "toolkit must come before the closing instruction"
        );
    }

    #[test]
    fn prompt_omits_toolkit_when_flag_is_false() {
        let prompt = build_prompt("Axum", "axum", "q", None, None, None, None, false, false);
        assert!(!prompt.contains("BEGIN EXPLORATION TOOLKIT"));
        assert!(!prompt.contains("kcl explore tree"));
    }

    #[test]
    fn prompt_injects_shallow_note_only_when_shallow() {
        let note = "this repository is a shallow clone";
        // shallow = true: the deepen-permission note is present.
        let with = build_prompt("Axum", "axum", "q", None, None, None, None, false, true);
        assert!(with.contains(note));
        assert!(with.contains("git fetch --unshallow"));
        // shallow = false: the note is absent.
        let without = build_prompt("Axum", "axum", "q", None, None, None, None, false, false);
        assert!(!without.contains(note));
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
            model_args: vec![],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let args = build_args(&harness, "test prompt", None, false);
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
            model_args: vec![],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let args = build_args(&harness, "my question", None, false);
        assert_eq!(args, vec!["--query=my question", "--title=my question"]);
    }

    #[test]
    fn build_args_stdin_mode_no_replacement() {
        let harness = HarnessConfig {
            command: "my-agent".to_string(),
            args: vec!["--query".to_string()],
            prompt_mode: PromptMode::Stdin,
            model_args: vec![],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let args = build_args(&harness, "test prompt", None, false);
        assert_eq!(args, vec!["--query"]);
        // {prompt} should NOT appear in args for stdin mode
        assert!(!args.iter().any(|a| a.contains("{prompt}")));
    }

    #[test]
    fn build_args_appends_model_when_supported() {
        let harness = HarnessConfig {
            command: "claude".to_string(),
            args: vec!["-p".to_string(), "{prompt}".to_string()],
            prompt_mode: PromptMode::Arg,
            model_args: vec!["--model".to_string(), "{model}".to_string()],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let args = build_args(&harness, "the question", Some("claude-sonnet-4.6"), false);
        assert_eq!(
            args,
            vec!["-p", "the question", "--model", "claude-sonnet-4.6"]
        );
    }

    #[test]
    fn build_args_silently_ignores_model_when_unsupported() {
        let harness = HarnessConfig {
            command: "pi".to_string(),
            args: vec!["-p".to_string(), "{prompt}".to_string()],
            prompt_mode: PromptMode::Arg,
            model_args: vec![], // empty: harness has no model flag
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let args = build_args(&harness, "q", Some("some-model"), false);
        assert_eq!(args, vec!["-p", "q"]);
    }

    #[test]
    fn build_args_does_not_inject_model_args_when_none_requested() {
        let harness = HarnessConfig {
            command: "claude".to_string(),
            args: vec!["-p".to_string(), "{prompt}".to_string()],
            prompt_mode: PromptMode::Arg,
            model_args: vec!["--model".to_string(), "{model}".to_string()],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let args = build_args(&harness, "q", None, false);
        assert_eq!(args, vec!["-p", "q"]);
    }

    #[test]
    fn build_args_model_combined_arg_format() {
        let harness = HarnessConfig {
            command: "agent".to_string(),
            args: vec!["{prompt}".to_string()],
            prompt_mode: PromptMode::Arg,
            model_args: vec!["--model={model}".to_string()],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let args = build_args(&harness, "q", Some("gpt-4"), false);
        assert_eq!(args, vec!["q", "--model=gpt-4"]);
    }

    #[test]
    fn build_args_appends_prepared_args_only_when_map_present() {
        let harness = HarnessConfig {
            command: "claude".to_string(),
            args: vec!["-p".to_string(), "{prompt}".to_string()],
            prompt_mode: PromptMode::Arg,
            model_args: vec![],
            default_model: None,
            prepared_args: vec!["--max-turns".to_string(), "8".to_string()],
            inject_explore_toolkit: true,
        };

        // No map: prepared_args are NOT appended (byte-for-byte unchanged).
        let no_map = build_args(&harness, "q", None, false);
        assert_eq!(no_map, vec!["-p", "q"]);

        // Map present: prepared_args appended last.
        let with_map = build_args(&harness, "q", None, true);
        assert_eq!(with_map, vec!["-p", "q", "--max-turns", "8"]);
    }

    #[test]
    fn build_args_map_present_but_no_prepared_args_is_unchanged() {
        let harness = HarnessConfig {
            command: "claude".to_string(),
            args: vec!["-p".to_string(), "{prompt}".to_string()],
            prompt_mode: PromptMode::Arg,
            model_args: vec![],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true, // none configured
        };

        let with_map = build_args(&harness, "q", None, true);
        let without_map = build_args(&harness, "q", None, false);
        assert_eq!(with_map, without_map);
        assert_eq!(with_map, vec!["-p", "q"]);
    }

    #[test]
    fn build_args_prepared_args_substitutes_prompt_and_model_placeholders() {
        let harness = HarnessConfig {
            command: "agent".to_string(),
            args: vec!["{prompt}".to_string()],
            prompt_mode: PromptMode::Arg,
            model_args: vec!["--model={model}".to_string()],
            default_model: None,
            prepared_args: vec![
                "--ceiling-for={model}".to_string(),
                "--echo={prompt}".to_string(),
            ],
            inject_explore_toolkit: true,
        };

        let args = build_args(&harness, "the q", Some("opus"), true);
        assert_eq!(
            args,
            vec![
                "the q",
                "--model=opus",
                "--ceiling-for=opus",
                "--echo=the q"
            ]
        );
    }

    #[test]
    fn build_args_prepared_args_no_prompt_substitution_in_stdin_mode() {
        // In stdin mode `args` never get `{prompt}` substituted; prepared_args
        // mirror that. `{model}` still substitutes when a model is resolved.
        let harness = HarnessConfig {
            command: "my-agent".to_string(),
            args: vec!["--query".to_string()],
            prompt_mode: PromptMode::Stdin,
            model_args: vec![],
            default_model: None,
            prepared_args: vec!["--keep={prompt}".to_string(), "--m={model}".to_string()],
            inject_explore_toolkit: true,
        };

        let args = build_args(&harness, "secret prompt", Some("haiku"), true);
        assert_eq!(args, vec!["--query", "--keep={prompt}", "--m=haiku"]);
    }

    #[tokio::test]
    async fn run_harness_captures_stdout_and_exit_code() {
        let harness = HarnessConfig {
            command: "echo".to_string(),
            args: vec!["hello from harness".to_string()],
            prompt_mode: PromptMode::Arg,
            model_args: vec![],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "", &cwd, 10, false, None)
            .await
            .unwrap();

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "hello from harness");
    }

    #[tokio::test]
    async fn run_harness_returns_nonzero_exit_code() {
        let harness = HarnessConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "exit 42".to_string()],
            prompt_mode: PromptMode::Arg,
            model_args: vec![],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "", &cwd, 10, false, None)
            .await
            .unwrap();

        assert_eq!(result.exit_code, Some(42));
    }

    #[tokio::test]
    async fn run_harness_captures_stderr() {
        let harness = HarnessConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo out; echo err >&2".to_string()],
            prompt_mode: PromptMode::Arg,
            model_args: vec![],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "", &cwd, 10, false, None)
            .await
            .unwrap();

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
            model_args: vec![],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "hello via stdin", &cwd, 10, false, None)
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
            model_args: vec![],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "the prompt text", &cwd, 10, false, None)
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
            model_args: vec![],
            default_model: None,
            prepared_args: vec![],
            inject_explore_toolkit: true,
        };

        let cwd = std::env::temp_dir();
        let result = run_harness(&harness, "", &cwd, 1, false, None).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    // ---------- new prompt injection tests for prepared maps + similar memories ----------

    #[test]
    fn prompt_injects_prepared_map_block_with_staleness() {
        use chrono::Utc;
        let p = PreparedContext {
            id: "p1".into(),
            package_id: "pkg".into(),
            content: "Key files: src/lib.rs\nStart here for routing.".into(),
            created_at: Utc::now(),
            harness: "claude".into(),
            model: None,
            git_commit_sha: Some("abc123def456".into()),
            git_branch: None,
            prepare_scope_at_time: "global".into(),
        };
        let prompt = build_prompt(
            "Axum",
            "axum",
            "How does X work?",
            None,
            Some(&p),
            Some("abc123def456"),
            Some(0),
            false,
            false,
        );

        assert!(prompt.contains("--- BEGIN PREPARED ORIENTATION MAP ---"));
        assert!(prompt.contains("Prepared at "));
        assert!(
            prompt.contains(
                "on commit `abc123def456` (current HEAD `abc123def456`). Scope: `global`"
            )
        );
        assert!(prompt.contains("Key files: src/lib.rs"));
        assert!(prompt.contains("--- END PREPARED ORIENTATION MAP ---"));
        // Authoritative directive line injected at the top of the map block.
        assert!(
            prompt.contains(
                "This is an accurate, authoritative orientation for this exact codebase."
            )
        );
        // Fresh map (0 commits behind): trust fully, no tree scan.
        assert!(prompt.contains("Do NOT perform a broad tree scan"));
        assert!(prompt.contains("authoritative primary source"));
        assert!(!prompt.contains("a few commits behind"));
        assert!(!prompt.contains("Explore the codebase and answer precisely"));
    }

    fn sample_prepared() -> PreparedContext {
        use chrono::Utc;
        PreparedContext {
            id: "p1".into(),
            package_id: "pkg".into(),
            content: "Key files: src/lib.rs".into(),
            created_at: Utc::now(),
            harness: "claude".into(),
            model: None,
            git_commit_sha: Some("abc123".into()),
            git_branch: None,
            prepare_scope_at_time: "global".into(),
        }
    }

    #[test]
    fn prompt_closing_unconditional_when_no_map() {
        let prompt = build_prompt("Axum", "axum", "q", None, None, None, None, false, false);
        assert!(
            prompt.contains("Explore the codebase and answer precisely. Reference file paths.")
        );
        assert!(!prompt.contains("authoritative primary source"));
    }

    #[test]
    fn prompt_closing_slightly_stale_verifies_affected_files() {
        let p = sample_prepared();
        let prompt = build_prompt(
            "Axum",
            "axum",
            "q",
            None,
            Some(&p),
            Some("def456"),
            Some(3),
            false,
            false,
        );
        assert!(prompt.contains("a few commits behind"));
        assert!(prompt.contains("verify only the specific files plausibly affected"));
        assert!(prompt.contains("Do NOT perform a broad tree scan"));
        // P2: a stale map must NOT be announced as authoritative for the exact
        // current codebase; the directive is softened to match the closing.
        assert!(!prompt.contains("authoritative orientation for this exact codebase"));
        assert!(prompt.contains("reliable guide to where things live"));
    }

    #[test]
    fn prompt_closing_unknown_staleness_falls_back_to_cautious() {
        let p = sample_prepared();
        let prompt = build_prompt(
            "Axum",
            "axum",
            "q",
            None,
            Some(&p),
            Some("def456"),
            None,
            false,
            false,
        );
        assert!(prompt.contains("may be stale"));
        assert!(prompt.contains("explore the codebase to verify and answer precisely"));
        assert!(!prompt.contains("Do NOT perform a broad tree scan"));
        // P2: unknown-age map is not authoritative; directive is softened.
        assert!(!prompt.contains("authoritative orientation for this exact codebase"));
        assert!(prompt.contains("may be out of date"));
    }

    #[test]
    fn prompt_closing_very_stale_falls_back_to_cautious() {
        let p = sample_prepared();
        let prompt = build_prompt(
            "Axum",
            "axum",
            "q",
            None,
            Some(&p),
            Some("def456"),
            Some(99),
            false,
            false,
        );
        assert!(prompt.contains("may be stale"));
        assert!(!prompt.contains("Do NOT perform a broad tree scan"));
        // P2: very-stale map (99 behind) → Unknown, never authoritative.
        assert!(!prompt.contains("authoritative orientation for this exact codebase"));
        assert!(prompt.contains("may be out of date"));
    }

    #[test]
    fn map_freshness_classify_boundaries() {
        use MapFreshness::*;
        assert_eq!(MapFreshness::classify(Some(0)), Fresh);
        assert_eq!(MapFreshness::classify(Some(1)), SlightlyStale);
        assert_eq!(
            MapFreshness::classify(Some(SLIGHTLY_STALE_MAX_COMMITS)),
            SlightlyStale
        );
        assert_eq!(
            MapFreshness::classify(Some(SLIGHTLY_STALE_MAX_COMMITS + 1)),
            Unknown
        );
        assert_eq!(MapFreshness::classify(None), Unknown);
    }
}
