//! Per-language import resolution.
//!
//! `outline_imports` rows record the raw `target` string written in source
//! (e.g. `"crate::foo::Bar"`, `"./baz"`, `"github.com/x/y/z"`). The `deps`
//! and `circular` commands need to follow these as edges in a file-to-file
//! graph — that requires turning each raw target into a concrete file under
//! the same package root.
//!
//! [`resolve_import`] handles that translation per-language. It is
//! best-effort: external crates, stdlib imports, and broken paths return
//! `None`. The caller (see `index_file` in `explore_index/mod.rs`) still
//! records a row in `outline_deps` for unresolved imports (with
//! `importee_file = NULL`) so the agent can see what couldn't be resolved.
//!
//! Resolution rules per language are summarized in each helper's doc comment.
//! The hard cases (`use a::b::Trait;` where `Trait` is a symbol, not a path
//! segment; `from .foo import bar` walking parent packages; Go module paths
//! from `go.mod`) are covered.

use std::path::{Path, PathBuf};

use super::parser::Language;

/// Try to resolve `raw_target` (as written in source) to a concrete file
/// under `root`. The returned path is relative to `root` and the file is
/// guaranteed to exist on disk at the time of the call.
///
/// Returns `None` for external crates / stdlib / broken paths — the indexer
/// still records these in `outline_deps` with `importee_file = NULL`.
pub fn resolve_import(
    root: &Path,
    importer_file: &Path,
    language: Language,
    raw_target: &str,
) -> Option<PathBuf> {
    let raw = raw_target.trim();
    if raw.is_empty() {
        return None;
    }

    match language {
        Language::Rust => resolve_rust(root, importer_file, raw),
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            resolve_jsts(root, importer_file, raw)
        }
        Language::Python => resolve_python(root, importer_file, raw),
        Language::Go => resolve_go(root, importer_file, raw),
        Language::C | Language::Cpp => resolve_c_like(root, importer_file, raw),
        Language::Unknown => None,
    }
}

// ---------- Rust ----------

/// Resolution rules:
/// - The raw target is the body of a `use` declaration as captured by the
///   parser: things like `"std::path::Path"`, `"crate::foo::Bar"`,
///   `"super::sibling::thing"`, `"self::child"`, `"foo::bar::Baz"`, or
///   group/glob/aliased imports.
/// - We strip group/alias decoration, then split on `::`. The last segment is
///   often a *symbol* (`Path`, `Bar`) not a module — we walk progressively
///   shorter prefixes against the filesystem.
/// - `crate::` and bare `foo::bar` both map to `src/foo/bar.rs` or
///   `src/foo/bar/mod.rs` (approximating the crate root as the directory
///   containing `Cargo.toml`, falling back to `src/`).
/// - `super::` and `self::` resolve relative to the importer's module
///   directory.
/// - `mod foo;` declarations also flow through here when written as bare
///   `foo` — they resolve to a sibling `foo.rs` or `foo/mod.rs`.
fn resolve_rust(root: &Path, importer: &Path, raw: &str) -> Option<PathBuf> {
    // Normalize: strip leading `pub `, leading `use ` (defensive — parser
    // already trims these), trailing `;`, and any `as Alias` clause.
    let cleaned = raw
        .trim_start_matches("pub ")
        .trim_start_matches("pub(crate) ")
        .trim_start_matches("pub(super) ")
        .trim_start_matches("use ")
        .trim_end_matches(';')
        .trim();

    // If it's a group import like `foo::{bar, baz}`, take just the path
    // prefix before the brace — we resolve to the directory/module.
    let cleaned = match cleaned.find('{') {
        Some(brace) => cleaned[..brace].trim_end_matches("::"),
        None => cleaned,
    };

    // Drop trailing `as Alias`.
    let cleaned = match cleaned.find(" as ") {
        Some(i) => &cleaned[..i],
        None => cleaned,
    };

    if cleaned.is_empty() {
        return None;
    }

    // Split into segments.
    let mut segs: Vec<&str> = cleaned.split("::").map(|s| s.trim()).collect();
    if segs.is_empty() {
        return None;
    }

    // `*` glob means "all symbols from <prefix>"; drop the trailing `*`.
    if segs.last().map(|s| *s == "*").unwrap_or(false) {
        segs.pop();
    }
    if segs.is_empty() {
        return None;
    }

    // Determine the base directory + remaining segments.
    let crate_src = crate_src_dir(root);
    let importer_dir = importer
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();

    let (mut base, rest): (PathBuf, Vec<&str>) = match segs[0] {
        "crate" => (crate_src.clone(), segs[1..].to_vec()),
        "self" => (importer_dir.clone(), segs[1..].to_vec()),
        "super" => {
            // Each leading `super` pops one directory.
            let mut cursor = importer_dir.clone();
            let mut idx = 0usize;
            while idx < segs.len() && segs[idx] == "super" {
                cursor = match cursor.parent() {
                    Some(p) => p.to_path_buf(),
                    None => return None,
                };
                idx += 1;
            }
            (cursor, segs[idx..].to_vec())
        }
        _ => {
            // Bare leading segment: could be a crate name or a top-level
            // module under `src/`. Try src-relative first; the resolver
            // returns None for external crates that don't match.
            (crate_src.clone(), segs.clone())
        }
    };

    if rest.is_empty() {
        // Importing a module itself, e.g. `use crate::foo;` — look for
        // foo.rs / foo/mod.rs. Fall through with empty `rest` triggering a
        // try-base check below.
        let candidate_rs = base.with_extension("rs");
        if root.join(&candidate_rs).is_file() {
            return Some(candidate_rs);
        }
        let mod_rs = base.join("mod.rs");
        if root.join(&mod_rs).is_file() {
            return Some(mod_rs);
        }
        return None;
    }

    // Walk segments, accumulating onto `base`. After each push, also try
    // dropping the just-pushed segment and treating it as a symbol within
    // the previous base — this is the "use foo::bar::Baz;" case where Baz
    // is a type, not a module.
    let mut acc: Vec<String> = Vec::new();
    for seg in &rest {
        acc.push((*seg).to_string());
    }

    // Try progressively shorter prefixes of `acc` against `base`.
    while !acc.is_empty() {
        let mut candidate = base.clone();
        for s in &acc {
            candidate.push(s);
        }
        let rs = candidate.with_extension("rs");
        if root.join(&rs).is_file() {
            return Some(rs);
        }
        let mod_rs = candidate.join("mod.rs");
        if root.join(&mod_rs).is_file() {
            return Some(mod_rs);
        }
        acc.pop();
    }

    // Last resort: maybe the import was `use crate::Foo;` where Foo is a
    // type in `src/lib.rs` or `src/main.rs`. Check the base itself.
    let base_rs = base.with_extension("rs");
    if root.join(&base_rs).is_file() {
        return Some(base_rs);
    }
    let base_mod = base.join("mod.rs");
    if root.join(&base_mod).is_file() {
        return Some(base_mod);
    }

    // If first segment was a bare name (not crate/self/super) and didn't
    // resolve under src/, also try resolving it relative to the importer's
    // directory — covers `use sibling_mod::thing;`.
    if !matches!(segs[0], "crate" | "self" | "super") {
        base = importer_dir;
        let mut acc: Vec<String> = segs.iter().map(|s| (*s).to_string()).collect();
        while !acc.is_empty() {
            let mut candidate = base.clone();
            for s in &acc {
                candidate.push(s);
            }
            let rs = candidate.with_extension("rs");
            if root.join(&rs).is_file() {
                return Some(rs);
            }
            let mod_rs = candidate.join("mod.rs");
            if root.join(&mod_rs).is_file() {
                return Some(mod_rs);
            }
            acc.pop();
        }
    }

    None
}

/// Approximate the crate's source root. If `<root>/Cargo.toml` exists we use
/// `<root>/src/`; otherwise fall back to `<root>` itself. Workspace crates
/// nested under `root` are out of scope for this resolver.
fn crate_src_dir(root: &Path) -> PathBuf {
    if root.join("Cargo.toml").is_file() && root.join("src").is_dir() {
        return PathBuf::from("src");
    }
    PathBuf::new()
}

// ---------- TypeScript / TSX / JavaScript ----------

/// Resolution rules:
/// - The raw target is the full import statement string captured by the
///   parser (e.g. `import { y } from "../bar/baz"`,
///   `import x from './foo'`, `require('./qux')`). We first extract the
///   quoted module specifier.
/// - Only relative specifiers (`./` or `../`) are resolved; bare specifiers
///   (`react`, `@scope/pkg`, `lodash`) return `None`.
/// - For each candidate base, try suffixes: `.ts`, `.tsx`, `.js`, `.jsx`,
///   `.mjs`, then `/index.<ext>`.
fn resolve_jsts(root: &Path, importer: &Path, raw: &str) -> Option<PathBuf> {
    let spec = extract_js_specifier(raw)?;
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let importer_dir = importer.parent().unwrap_or(Path::new(""));
    let base = importer_dir.join(&spec);

    let suffixes = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];
    for suf in &suffixes {
        let candidate = append_extension(&base, suf);
        if root.join(&candidate).is_file() {
            return Some(normalize(&candidate));
        }
    }
    // If `base` is already a directory or matches a folder, try index files.
    for suf in [
        "index.ts",
        "index.tsx",
        "index.js",
        "index.jsx",
        "index.mjs",
    ] {
        let candidate = base.join(suf);
        if root.join(&candidate).is_file() {
            return Some(normalize(&candidate));
        }
    }
    // It's possible the spec already ends in a recognized extension.
    if root.join(&base).is_file() {
        return Some(normalize(&base));
    }
    None
}

fn append_extension(base: &Path, suf: &str) -> PathBuf {
    let mut s = base.to_string_lossy().into_owned();
    s.push_str(suf);
    PathBuf::from(s)
}

/// Pull the first quoted string out of a JS/TS import/require statement.
fn extract_js_specifier(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\'' || b == b'`' {
            let quote = b;
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != quote {
                j += 1;
            }
            if j < bytes.len() {
                return Some(raw[start..j].to_string());
            }
            return None;
        }
        i += 1;
    }
    None
}

/// Collapse `./a/../b` style paths textually (PathBuf doesn't do this for
/// us). Best-effort; we don't follow symlinks.
fn normalize(p: &Path) -> PathBuf {
    let mut out: Vec<std::path::Component> = Vec::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                if matches!(out.last(), Some(std::path::Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.iter().collect()
}

// ---------- Python ----------

/// Resolution rules:
/// - The raw target is the full `import ...` / `from ... import ...`
///   statement. We extract the module path.
/// - `from .x import y` is relative: count leading dots, walk up that many
///   parent directories from the importer, then descend into the rest of
///   the dotted path.
/// - `import a.b` maps to `a/b.py` or `a/b/__init__.py`, searched from the
///   "package root" — the topmost directory above the importer that still
///   contains `__init__.py`.
fn resolve_python(root: &Path, importer: &Path, raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();

    // Parse module path and leading-dot count.
    let (module_path, leading_dots): (String, usize) =
        if let Some(rest) = trimmed.strip_prefix("from ") {
            let head = rest.split_whitespace().next().unwrap_or("");
            let dots = head.chars().take_while(|c| *c == '.').count();
            let mod_part: String = head.chars().skip(dots).collect();
            (mod_part, dots)
        } else if let Some(rest) = trimmed.strip_prefix("import ") {
            // `import a.b, c.d` — just take the first.
            let first = rest
                .split(|c: char| c == ',' || c.is_whitespace())
                .next()
                .unwrap_or("");
            (first.to_string(), 0)
        } else {
            return None;
        };

    let importer_dir = importer
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();

    let base_dir: PathBuf = if leading_dots > 0 {
        // Relative: pop (leading_dots - 1) parents from importer_dir.
        let mut cur = importer_dir.clone();
        for _ in 1..leading_dots {
            cur = match cur.parent() {
                Some(p) => p.to_path_buf(),
                None => return None,
            };
        }
        cur
    } else {
        // Absolute: walk up from importer until we find a directory whose
        // parent is NOT a python package (no __init__.py). That's the
        // package root.
        python_package_root(root, &importer_dir)
    };

    let segs: Vec<&str> = module_path.split('.').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() && leading_dots == 0 {
        return None;
    }

    let mut candidate = base_dir.clone();
    for seg in &segs {
        candidate.push(seg);
    }

    // Try `foo.py` and `foo/__init__.py` for the full path.
    let with_py = candidate.with_extension("py");
    if root.join(&with_py).is_file() {
        return Some(with_py);
    }
    let init = candidate.join("__init__.py");
    if root.join(&init).is_file() {
        return Some(init);
    }

    // For `from x import y`, the last segment may be a symbol — drop one
    // and try again.
    if segs.len() > 1 {
        let mut shorter = base_dir.clone();
        for seg in &segs[..segs.len() - 1] {
            shorter.push(seg);
        }
        let py = shorter.with_extension("py");
        if root.join(&py).is_file() {
            return Some(py);
        }
        let init = shorter.join("__init__.py");
        if root.join(&init).is_file() {
            return Some(init);
        }
    }

    None
}

fn python_package_root(root: &Path, importer_dir: &Path) -> PathBuf {
    // Walk up from importer_dir until we find a directory whose parent does
    // not contain `__init__.py`. That directory's parent is the package root.
    let mut cur = importer_dir.to_path_buf();
    loop {
        let parent = match cur.parent() {
            Some(p) => p.to_path_buf(),
            None => return PathBuf::new(),
        };
        if !root.join(&parent).join("__init__.py").is_file() {
            return parent;
        }
        if parent.as_os_str().is_empty() {
            return PathBuf::new();
        }
        cur = parent;
    }
}

// ---------- Go ----------

/// Resolution rules:
/// - Read `<root>/go.mod` if present to recover the module path. If the
///   `import "..."` target starts with that path, strip it; the remainder
///   is a directory under root. Resolve to the first `.go` file in that
///   directory (excluding `_test.go`).
/// - Imports from outside the module return `None`.
fn resolve_go(root: &Path, _importer: &Path, raw: &str) -> Option<PathBuf> {
    // The captured raw_target for individual import_spec rows is the
    // unquoted module path; the multi-line `import ( ... )` row's target is
    // the first line. We only handle the unquoted single-spec form here.
    let module_path = raw.trim().trim_matches('"');
    if module_path.is_empty() || module_path.starts_with("import") {
        return None;
    }
    let module = read_go_mod(root)?;
    let rel = module_path.strip_prefix(&module)?;
    let rel = rel.trim_start_matches('/');
    let dir = PathBuf::from(rel);
    let abs_dir = root.join(&dir);
    if !abs_dir.is_dir() {
        return None;
    }
    // Pick the first non-test .go file as a canonical representative.
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&abs_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("go")
                && !p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.ends_with("_test.go"))
                    .unwrap_or(false)
        })
        .collect();
    entries.sort();
    let chosen = entries.into_iter().next()?;
    let rel_chosen = chosen.strip_prefix(root).ok()?.to_path_buf();
    Some(rel_chosen)
}

fn read_go_mod(root: &Path) -> Option<String> {
    let content = std::fs::read_to_string(root.join("go.mod")).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("module ") {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

// ---------- C / C++ ----------

/// Resolution rules:
/// - Only quoted (`"foo.h"`) `#include`s are resolved — system headers
///   (`<foo.h>`) always return `None`.
/// - The parser already strips the surrounding quotes, so the raw target is
///   the bare filename. We try:
///   - importer's directory (sibling)
///   - `<root>/include/`
///   - `<root>/src/`
fn resolve_c_like(root: &Path, importer: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    // System headers (which the parser passes through unwrapped) would
    // typically have no slashes and a `.h` suffix, but the parser DOES
    // distinguish `<...>` vs `"..."` by inspecting which delimiter was
    // present — and we drop both. To stay safe, we just try paths under
    // `root` and bail when nothing matches; system headers naturally won't
    // resolve to anything under `root`.
    if raw.is_empty() {
        return None;
    }
    let importer_dir = importer.parent().unwrap_or(Path::new(""));
    let candidates = [
        importer_dir.join(raw),
        PathBuf::from("include").join(raw),
        PathBuf::from("src").join(raw),
        PathBuf::from(raw),
    ];
    for c in &candidates {
        let nc = normalize(c);
        if root.join(&nc).is_file() {
            return Some(nc);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ---------- Rust ----------

    #[test]
    fn rust_crate_path_resolves_with_trailing_symbol() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"").unwrap();
        std::fs::create_dir_all(root.join("src/foo")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/foo/bar.rs"), "pub struct Baz;").unwrap();

        let resolved = resolve_import(
            root,
            Path::new("src/lib.rs"),
            Language::Rust,
            "crate::foo::bar::Baz",
        )
        .expect("should resolve to src/foo/bar.rs by stripping the trailing symbol");
        assert_eq!(resolved, PathBuf::from("src/foo/bar.rs"));
    }

    #[test]
    fn rust_super_and_self_resolve_relative_to_importer() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"").unwrap();
        std::fs::create_dir_all(root.join("src/a")).unwrap();
        std::fs::write(root.join("src/a/mod.rs"), "").unwrap();
        std::fs::write(root.join("src/a/sibling.rs"), "").unwrap();
        std::fs::write(root.join("src/top.rs"), "").unwrap();

        let from = Path::new("src/a/mod.rs");
        let r1 = resolve_import(root, from, Language::Rust, "self::sibling").unwrap();
        assert_eq!(r1, PathBuf::from("src/a/sibling.rs"));

        let r2 = resolve_import(root, from, Language::Rust, "super::top").unwrap();
        assert_eq!(r2, PathBuf::from("src/top.rs"));
    }

    #[test]
    fn rust_mod_rs_resolution() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(root.join("src/foo")).unwrap();
        std::fs::write(root.join("src/foo/mod.rs"), "").unwrap();

        let r =
            resolve_import(root, Path::new("src/lib.rs"), Language::Rust, "crate::foo").unwrap();
        assert_eq!(r, PathBuf::from("src/foo/mod.rs"));
    }

    #[test]
    fn rust_external_crate_returns_none() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();

        let r = resolve_import(
            root,
            Path::new("src/lib.rs"),
            Language::Rust,
            "std::path::Path",
        );
        assert!(r.is_none(), "std imports should not resolve under root");
    }

    #[test]
    fn rust_group_import_resolves_to_module() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/util.rs"), "").unwrap();

        let r = resolve_import(
            root,
            Path::new("src/lib.rs"),
            Language::Rust,
            "crate::util::{foo, bar}",
        )
        .unwrap();
        assert_eq!(r, PathBuf::from("src/util.rs"));
    }

    // ---------- TS/JS ----------

    #[test]
    fn ts_relative_with_extensions_tried_in_order() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.ts"), "").unwrap();
        std::fs::write(root.join("src/b.ts"), "").unwrap();

        let r = resolve_import(
            root,
            Path::new("src/a.ts"),
            Language::TypeScript,
            "import { x } from './b';",
        )
        .unwrap();
        assert_eq!(r, PathBuf::from("src/b.ts"));
    }

    #[test]
    fn ts_index_file_resolution() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        std::fs::write(root.join("src/a.ts"), "").unwrap();
        std::fs::write(root.join("src/sub/index.ts"), "").unwrap();

        let r = resolve_import(
            root,
            Path::new("src/a.ts"),
            Language::TypeScript,
            "import x from './sub';",
        )
        .unwrap();
        assert_eq!(r, PathBuf::from("src/sub/index.ts"));
    }

    #[test]
    fn ts_bare_specifier_returns_none() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.ts"), "").unwrap();

        let r = resolve_import(
            root,
            Path::new("src/a.ts"),
            Language::TypeScript,
            "import React from 'react';",
        );
        assert!(r.is_none());
    }

    // ---------- Python ----------

    #[test]
    fn python_relative_from_dot_import() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/__init__.py"), "").unwrap();
        std::fs::write(root.join("pkg/a.py"), "").unwrap();
        std::fs::write(root.join("pkg/b.py"), "").unwrap();

        let r = resolve_import(
            root,
            Path::new("pkg/a.py"),
            Language::Python,
            "from .b import thing",
        )
        .unwrap();
        assert_eq!(r, PathBuf::from("pkg/b.py"));
    }

    #[test]
    fn python_absolute_import_with_symbol_fallback() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("pkg/sub")).unwrap();
        std::fs::write(root.join("pkg/__init__.py"), "").unwrap();
        std::fs::write(root.join("pkg/sub/__init__.py"), "").unwrap();
        std::fs::write(root.join("pkg/sub/util.py"), "").unwrap();

        let r = resolve_import(
            root,
            Path::new("pkg/sub/util.py"),
            Language::Python,
            "from pkg.sub.util import thing",
        )
        .unwrap();
        assert_eq!(r, PathBuf::from("pkg/sub/util.py"));
    }

    // ---------- Go ----------

    #[test]
    fn go_module_local_import() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), "module github.com/x/y\n").unwrap();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("main.go"), "package main\n").unwrap();
        std::fs::write(root.join("pkg/util.go"), "package pkg\n").unwrap();

        let r = resolve_import(
            root,
            Path::new("main.go"),
            Language::Go,
            "github.com/x/y/pkg",
        )
        .unwrap();
        assert_eq!(r, PathBuf::from("pkg/util.go"));
    }

    #[test]
    fn go_external_import_returns_none() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), "module example.com/foo\n").unwrap();
        let r = resolve_import(root, Path::new("main.go"), Language::Go, "fmt");
        assert!(r.is_none());
    }

    // ---------- C/C++ ----------

    #[test]
    fn c_quoted_include_resolves_to_sibling() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.c"), "").unwrap();
        std::fs::write(root.join("src/util.h"), "").unwrap();

        let r = resolve_import(root, Path::new("src/main.c"), Language::C, "util.h").unwrap();
        assert_eq!(r, PathBuf::from("src/util.h"));
    }

    #[test]
    fn c_include_dir_resolution() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("include")).unwrap();
        std::fs::write(root.join("src/main.c"), "").unwrap();
        std::fs::write(root.join("include/api.h"), "").unwrap();

        let r = resolve_import(root, Path::new("src/main.c"), Language::C, "api.h").unwrap();
        assert_eq!(r, PathBuf::from("include/api.h"));
    }
}
