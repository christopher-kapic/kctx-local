//! `kcl explore read` — read a line range from a file with a content hash.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::util::{json_or_text, resolve_path, resolve_target};

#[derive(Debug, Serialize)]
struct LineOut {
    n: usize,
    text: String,
}

#[derive(Debug, Serialize)]
struct ReadOutput {
    path: String,
    total_lines: usize,
    returned_range: [usize; 2],
    content_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    lines: Option<Vec<LineOut>>,
    /// True when `--no-line-numbers` was set so the text renderer drops the
    /// numeric prefix. Skipped in JSON output to keep the shape minimal.
    #[serde(skip)]
    no_line_numbers: bool,
}

pub fn run(
    file: &Path,
    start: Option<usize>,
    end: Option<usize>,
    no_line_numbers: bool,
    package: Option<&str>,
    json: bool,
    max_bytes: usize,
) -> Result<i32> {
    let target = resolve_target(package)?;
    let resolved = resolve_path(&target, file);

    let bytes =
        fs::read(&resolved).with_context(|| format!("reading file `{}`", resolved.display()))?;

    // SHA-256 of the full file (hex).
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let content_hash = hex_encode(&hasher.finalize());

    // Split the file into lines preserving content (best-effort UTF-8 view).
    let text = String::from_utf8_lossy(&bytes);
    let all_lines: Vec<&str> = text.lines().collect();
    let total_lines = all_lines.len();

    if total_lines == 0 {
        let output = ReadOutput {
            path: rel_path(&target.root, &resolved),
            total_lines: 0,
            returned_range: [0, 0],
            content_hash,
            lines: if json { Some(Vec::new()) } else { None },
            no_line_numbers,
        };
        json_or_text(json, &output, max_bytes, render_text)?;
        return Ok(0);
    }

    let s = start.unwrap_or(1).max(1);
    let e = end.unwrap_or(total_lines).min(total_lines).max(s);

    // Slice inclusive [s, e] in 1-indexed terms.
    let from = s - 1;
    let to = e; // exclusive upper bound

    let selected: Vec<LineOut> = all_lines[from..to]
        .iter()
        .enumerate()
        .map(|(i, line)| LineOut {
            n: from + 1 + i,
            text: (*line).to_string(),
        })
        .collect();

    let output = ReadOutput {
        path: rel_path(&target.root, &resolved),
        total_lines,
        returned_range: [s, e],
        content_hash,
        lines: Some(selected),
        no_line_numbers,
    };

    json_or_text(json, &output, max_bytes, render_text)?;
    Ok(0)
}

fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn render_text(out: &ReadOutput) -> String {
    let mut s = String::new();
    // Header: a single JSON line so the harness can parse provenance without
    // pulling the body.
    let header = serde_json::json!({
        "path": out.path,
        "total_lines": out.total_lines,
        "returned_range": out.returned_range,
        "content_hash": out.content_hash,
    });
    s.push_str(&header.to_string());
    s.push('\n');

    let pad = digit_width(out.total_lines.max(1));
    if let Some(lines) = &out.lines {
        for line in lines {
            if out.no_line_numbers {
                s.push_str(&line.text);
            } else {
                s.push_str(&format!("{:>width$}: {}", line.n, line.text, width = pad));
            }
            s.push('\n');
        }
    }
    s
}

fn digit_width(n: usize) -> usize {
    let mut n = n;
    let mut w = 1;
    while n >= 10 {
        n /= 10;
        w += 1;
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encode_matches_known_vectors() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let mut h = Sha256::new();
        h.update(b"");
        assert_eq!(
            hex_encode(&h.finalize()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn digit_width_is_correct() {
        assert_eq!(digit_width(1), 1);
        assert_eq!(digit_width(9), 1);
        assert_eq!(digit_width(10), 2);
        assert_eq!(digit_width(999), 3);
        assert_eq!(digit_width(1000), 4);
    }
}
