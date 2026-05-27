//! Shared helpers for `kcl explore` subcommands: package/cwd resolution,
//! byte-budgeted writer, and a small json-or-text print helper.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::db;
use crate::models::package::Package;
use crate::paths;

/// The package/path context an `explore` subcommand operates against.
///
/// - `root` is the absolute, canonicalized package root. All path arguments
///   passed to a subcommand are resolved relative to this when they are
///   relative paths.
/// - `id` and `display_name` are `Some` only when `root` matches a registered
///   package. When the CLI was invoked from a directory that is not (under)
///   a registered package, `root = $PWD` and the identifying fields are `None`.
#[derive(Debug, Clone)]
pub struct ExploreTarget {
    /// Registered package identifier, if matched.
    pub id: Option<String>,
    /// Package display_name, if matched. Currently unread by Phase 1 commands
    /// but exposed for Phase 2+ (e.g. richer headers, prepared-map injection).
    #[allow(dead_code)]
    pub display_name: Option<String>,
    /// Absolute, canonicalized root directory.
    pub root: PathBuf,
}

/// Resolve which package/path an `explore` command should operate on.
///
/// Resolution order:
/// 1. If `package_id` is `Some`: look up the package in the DB. Error if not
///    found (per the project's backtick-delimited message style).
/// 2. Otherwise: canonicalize `$PWD` and walk up its ancestors looking for one
///    whose canonical path equals a registered package's canonical path. If
///    matched, return that.
/// 3. Otherwise: return `$PWD` with `id = None` (anonymous mode).
pub fn resolve_target(package_id: Option<&str>) -> Result<ExploreTarget> {
    // Step 1: explicit override.
    if let Some(id) = package_id {
        let db_path = paths::db_file()?;
        let conn = db::open(&db_path)?;
        let pkg = Package::get_by_identifier(&conn, id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "Package `{}` not found. Run `kcl list` to see available packages.",
                id
            )
        })?;
        let root = canonicalize_or_self(Path::new(&pkg.path));
        return Ok(ExploreTarget {
            id: Some(pkg.identifier),
            display_name: Some(pkg.display_name),
            root,
        });
    }

    // Step 2/3: try to match $PWD (or one of its ancestors) against a
    // registered package.
    let cwd = std::env::current_dir().context("could not determine current directory")?;
    let cwd = canonicalize_or_self(&cwd);

    // Open the DB lazily — if it doesn't exist yet, fall back to anonymous mode.
    let db_path = paths::db_file()?;
    let packages: Vec<Package> = if db_path.exists() {
        match db::open(&db_path) {
            Ok(conn) => Package::list_all(&conn).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let canonicals: Vec<(PathBuf, &Package)> = packages
        .iter()
        .map(|p| (canonicalize_or_self(Path::new(&p.path)), p))
        .collect();

    for ancestor in cwd.ancestors() {
        for (canon, pkg) in &canonicals {
            if canon == ancestor {
                return Ok(ExploreTarget {
                    id: Some(pkg.identifier.clone()),
                    display_name: Some(pkg.display_name.clone()),
                    root: canon.clone(),
                });
            }
        }
    }

    Ok(ExploreTarget {
        id: None,
        display_name: None,
        root: cwd,
    })
}

/// Best-effort canonicalize: fall back to the original path if the call fails
/// (e.g. the file doesn't yet exist or a symlink can't be resolved).
fn canonicalize_or_self(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Resolve a user-supplied (possibly relative) path against the target root.
///
/// If `p` is absolute it's returned canonicalized as-is; otherwise it's joined
/// onto `target.root` first.
pub fn resolve_path(target: &ExploreTarget, p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        target.root.join(p)
    };
    canonicalize_or_self(&joined)
}

/// A `Write` implementation that stops writing once `budget` bytes have been
/// emitted. Subsequent writes are silently dropped and `truncated()` returns
/// true so the caller can decide how to annotate the output.
///
/// The byte count is exact: a write that would only partially fit is rejected
/// entirely (its bytes are counted as not-written, and `truncated` flips on).
/// This keeps the on-the-wire output a valid prefix of the intended bytes
/// without splitting UTF-8 sequences.
pub struct BudgetedWriter<W: Write> {
    inner: W,
    budget: usize,
    written: usize,
    truncated: bool,
}

impl<W: Write> BudgetedWriter<W> {
    pub fn new(inner: W, budget: usize) -> Self {
        Self {
            inner,
            budget,
            written: 0,
            truncated: false,
        }
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Attempt to write a string; returns Ok(()) even when the write was
    /// dropped due to the budget (`truncated()` will return true). Real I/O
    /// errors are propagated.
    pub fn write_str(&mut self, s: &str) -> std::io::Result<()> {
        if self.truncated {
            return Ok(());
        }
        let remaining = self.budget.saturating_sub(self.written);
        if s.len() > remaining {
            self.truncated = true;
            return Ok(());
        }
        self.inner.write_all(s.as_bytes())?;
        self.written += s.len();
        Ok(())
    }
}

/// Print `value` to stdout as either JSON or rendered text, capped at
/// `max_bytes`. The truncation marker:
/// - In JSON mode: an object with `{truncated: true, bytes: <max>}` printed on
///   its own line *after* the (possibly partial) payload.
/// - In text mode: a final line `... [truncated at <N> bytes]`.
pub fn json_or_text<T, F>(json: bool, value: &T, max_bytes: usize, render_text: F) -> Result<()>
where
    T: Serialize,
    F: FnOnce(&T) -> String,
{
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let mut bw = BudgetedWriter::new(&mut handle, max_bytes);

    if json {
        let s = serde_json::to_string(value).context("serializing explore output as JSON")?;
        bw.write_str(&s)?;
        bw.write_str("\n")?;
    } else {
        let s = render_text(value);
        bw.write_str(&s)?;
        if !s.ends_with('\n') {
            bw.write_str("\n")?;
        }
    }

    if bw.truncated() {
        if json {
            // Emit a structured truncation marker on its own line.
            let marker = serde_json::json!({ "truncated": true, "bytes": max_bytes });
            // Bypass the budget for the marker — it's the explicit signal.
            handle
                .write_all(marker.to_string().as_bytes())
                .context("writing truncation marker")?;
            handle
                .write_all(b"\n")
                .context("writing truncation marker newline")?;
        } else {
            let marker = format!("... [truncated at {} bytes]\n", max_bytes);
            handle
                .write_all(marker.as_bytes())
                .context("writing truncation marker")?;
        }
    }
    Ok(())
}

/// Best-effort guess of a file's language based on its extension. Used for
/// per-file annotations in `tree`. Returns `None` for unrecognized extensions.
pub fn language_for(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "c" => "c",
        "h" => "c-header",
        "hpp" | "hh" | "hxx" => "cpp-header",
        "cpp" | "cc" | "cxx" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "scala" => "scala",
        "sh" | "bash" | "zsh" => "shell",
        "fish" => "fish",
        "lua" => "lua",
        "pl" => "perl",
        "r" => "r",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "clj" | "cljs" | "cljc" => "clojure",
        "dart" => "dart",
        "zig" => "zig",
        "nim" => "nim",
        "sql" => "sql",
        "md" | "markdown" => "markdown",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "vue" => "vue",
        "svelte" => "svelte",
        "proto" => "protobuf",
        "graphql" | "gql" => "graphql",
        "dockerfile" => "dockerfile",
        "tf" => "terraform",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn budgeted_writer_writes_under_budget() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut bw = BudgetedWriter::new(&mut buf, 100);
            bw.write_str("hello ").unwrap();
            bw.write_str("world").unwrap();
            assert!(!bw.truncated());
        }
        assert_eq!(buf, b"hello world");
    }

    #[test]
    fn budgeted_writer_drops_oversized_write_atomically() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut bw = BudgetedWriter::new(&mut buf, 5);
            bw.write_str("hi ").unwrap();
            // "longer" wouldn't fit; entire chunk is dropped, not split.
            bw.write_str("longer").unwrap();
            assert!(bw.truncated());
            // After truncation flips on, further writes are silent no-ops.
            bw.write_str("ignored").unwrap();
            assert!(bw.truncated());
        }
        assert_eq!(buf, b"hi ");
    }

    #[test]
    fn budgeted_writer_zero_budget_truncates_first_write() {
        let mut buf: Vec<u8> = Vec::new();
        let mut bw = BudgetedWriter::new(Cursor::new(&mut buf), 0);
        bw.write_str("anything").unwrap();
        assert!(bw.truncated());
        assert!(buf.is_empty());
    }

    #[test]
    fn language_for_handles_common_extensions() {
        assert_eq!(language_for(Path::new("a.rs")), Some("rust"));
        assert_eq!(language_for(Path::new("a.PY")), Some("python"));
        assert_eq!(language_for(Path::new("a.tsx")), Some("tsx"));
        assert_eq!(language_for(Path::new("a.unknownext")), None);
        assert_eq!(language_for(Path::new("noext")), None);
    }
}
