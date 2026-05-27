//! `kcl explore search` — content search via ripgrep, budget-capped.
//!
//! Shells out to `rg --json` and parses the stream into structured matches.
//! Both text and JSON output are emitted via the shared budget-capped printer.

use std::collections::HashMap;
use std::process::Stdio;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::util::{json_or_text, resolve_target};

#[derive(Debug, Serialize)]
struct MatchOut {
    path: String,
    line: u64,
    column: u64,
    match_text: String,
    before: Vec<ContextLine>,
    after: Vec<ContextLine>,
}

#[derive(Debug, Serialize)]
struct ContextLine {
    line: u64,
    text: String,
}

#[derive(Debug, Default)]
struct PendingFile {
    path: String,
    /// All context lines emitted by rg for this file, keyed by line number.
    /// We partition them into before/after per match in `finalize_file`.
    context: HashMap<u64, String>,
    matches: Vec<RawMatch>,
}

#[derive(Debug)]
struct RawMatch {
    line: u64,
    column: u64,
    text: String,
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    pattern: &str,
    type_filter: Option<&str>,
    path_glob: Option<&str>,
    context: usize,
    case_insensitive: bool,
    package: Option<&str>,
    json: bool,
    max_bytes: usize,
) -> Result<i32> {
    let target = resolve_target(package)?;

    // Verify rg is on PATH so we can give a helpful error rather than a
    // raw spawn failure.
    if which::which("rg").is_err() {
        anyhow::bail!(
            "ripgrep (`rg`) was not found on PATH. Install it (`brew install ripgrep`, `apt install ripgrep`, etc.) to use `kcl explore search`."
        );
    }

    let mut cmd = Command::new("rg");
    cmd.arg("--json")
        .arg("--line-number")
        .arg("--column")
        .arg("--no-heading")
        .arg("--color")
        .arg("never");
    if context > 0 {
        cmd.arg("--context").arg(context.to_string());
    }
    if case_insensitive {
        cmd.arg("--ignore-case");
    }
    if let Some(t) = type_filter {
        cmd.arg("--type").arg(t);
    }
    if let Some(g) = path_glob {
        cmd.arg("--glob").arg(g);
    }
    cmd.arg("--").arg(pattern).arg(".");
    cmd.current_dir(&target.root);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawning `rg`")?;
    let stdout = child.stdout.take().expect("rg stdout should be piped");
    let stderr = child.stderr.take().expect("rg stderr should be piped");

    let mut reader = BufReader::new(stdout).lines();
    let mut current = PendingFile::default();
    let mut all: Vec<MatchOut> = Vec::new();

    while let Some(line) = reader.next_line().await? {
        if line.is_empty() {
            continue;
        }
        let evt: RgEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue, // unknown event shape; skip defensively
        };
        match evt {
            RgEvent::Begin { data } => {
                current = PendingFile {
                    path: data.path.text.unwrap_or_default(),
                    ..Default::default()
                };
            }
            RgEvent::Match { data } => {
                let text = data.lines.text.unwrap_or_default();
                let col = data
                    .submatches
                    .first()
                    .map(|sm| sm.start as u64 + 1)
                    .unwrap_or(1);
                current.matches.push(RawMatch {
                    line: data.line_number,
                    column: col,
                    text,
                });
            }
            RgEvent::Context { data } => {
                let text = data.lines.text.unwrap_or_default();
                current.context.insert(data.line_number, text);
            }
            RgEvent::End => {
                finalize_file(&mut current, context, &mut all);
            }
            RgEvent::Other => {}
        }
    }

    // Drain stderr (rg writes warnings here). We don't surface it line-by-line
    // to stdout — the harness wants clean JSON or text on stdout.
    let mut stderr_buf = String::new();
    let mut stderr_reader = BufReader::new(stderr).lines();
    while let Some(line) = stderr_reader.next_line().await? {
        if !stderr_buf.is_empty() {
            stderr_buf.push('\n');
        }
        stderr_buf.push_str(&line);
    }

    let status = child.wait().await.context("waiting on `rg`")?;

    // rg exit codes: 0 = matches, 1 = no matches, 2 = error.
    let code = status.code().unwrap_or(2);
    if code == 2 {
        if !stderr_buf.is_empty() {
            eprintln!("rg: {}", stderr_buf.trim_end());
        }
        return Ok(3);
    }

    json_or_text(json, &all, max_bytes, render_text)?;
    Ok(0)
}

/// Walk a file's pending matches and partition the recorded context lines
/// (from `Context` events) into before/after for each match. ripgrep emits
/// context lines inline between matches; we just bucket them by line number.
fn finalize_file(current: &mut PendingFile, context_lines: usize, out: &mut Vec<MatchOut>) {
    let ctx = std::mem::take(&mut current.context);
    let matches = std::mem::take(&mut current.matches);

    for m in matches {
        let mut before = Vec::with_capacity(context_lines);
        let mut after = Vec::with_capacity(context_lines);
        for k in 1..=context_lines as u64 {
            if let Some(line_no) = m.line.checked_sub(k)
                && let Some(t) = ctx.get(&line_no)
            {
                before.push(ContextLine {
                    line: line_no,
                    text: t.clone(),
                });
            }
            let after_line = m.line + k;
            if let Some(t) = ctx.get(&after_line) {
                after.push(ContextLine {
                    line: after_line,
                    text: t.clone(),
                });
            }
        }
        before.reverse(); // ascending order
        out.push(MatchOut {
            path: current.path.clone(),
            line: m.line,
            column: m.column,
            match_text: m.text,
            before,
            after,
        });
    }
}

fn render_text(matches: &Vec<MatchOut>) -> String {
    if matches.is_empty() {
        return "(no matches)\n".to_string();
    }
    let mut s = String::new();
    let mut first = true;
    for m in matches {
        if !first {
            s.push_str("--\n");
        }
        first = false;
        for ctx in &m.before {
            s.push_str(&format!(
                "{}:{}: {}\n",
                m.path,
                ctx.line,
                ctx.text.trim_end_matches('\n')
            ));
        }
        s.push_str(&format!(
            "{}:{}:{}: {}\n",
            m.path,
            m.line,
            m.column,
            m.match_text.trim_end_matches('\n')
        ));
        for ctx in &m.after {
            s.push_str(&format!(
                "{}:{}: {}\n",
                m.path,
                ctx.line,
                ctx.text.trim_end_matches('\n')
            ));
        }
    }
    s
}

/// ripgrep's `--json` event stream is one JSON object per line; the `type` tag
/// distinguishes record shapes.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum RgEvent {
    Begin {
        data: BeginData,
    },
    Match {
        data: MatchData,
    },
    Context {
        data: ContextData,
    },
    End,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct BeginData {
    path: RgPath,
}

#[derive(Debug, Deserialize)]
struct MatchData {
    #[allow(dead_code)]
    path: RgPath,
    lines: RgLines,
    line_number: u64,
    submatches: Vec<RgSubmatch>,
}

#[derive(Debug, Deserialize)]
struct ContextData {
    #[allow(dead_code)]
    path: RgPath,
    lines: RgLines,
    line_number: u64,
}

#[derive(Debug, Deserialize)]
struct RgPath {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RgLines {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RgSubmatch {
    start: usize,
}
