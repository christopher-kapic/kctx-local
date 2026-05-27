//! Per-language tree-sitter parsers and symbol extraction.
//!
//! Each supported language has a small `extract_*` function that walks the
//! parsed tree and returns the symbol / import / identifier rows that the
//! outline index stores. We prefer tree-sitter `Query`s over hand-rolled node
//! walks where they're tractable; `extract_identifiers` is the one place we
//! traverse the full tree manually because we want every identifier-like node
//! kind regardless of context.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

/// Languages the outline indexer knows about. `Unknown` is what
/// [`Language::from_path`] returns for any extension we can't parse — those
/// files are skipped by indexing but remain visible to other explore commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    C,
    Cpp,
    Unknown,
}

impl Language {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())?
            .to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Language::Rust,
            "ts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "py" | "pyi" => Language::Python,
            "go" => Language::Go,
            "c" | "h" => Language::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
            Language::Python => "python",
            Language::Go => "go",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Unknown => "unknown",
        }
    }

    fn ts_language(self) -> Option<tree_sitter::Language> {
        Some(match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::Go => tree_sitter_go::LANGUAGE.into(),
            Language::C => tree_sitter_c::LANGUAGE.into(),
            Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Language::Unknown => return None,
        })
    }
}

/// A declared symbol extracted from a source file. `line` is 1-indexed.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub end_line: Option<u32>,
    pub parent: Option<String>,
    pub visibility: Option<String>,
    pub signature: Option<String>,
}

/// An import / module reference. `target` is the raw module/path string as
/// written in source — exactly what `use` / `import` / `#include` wrote.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Import {
    pub target: String,
    pub line: u32,
}

/// A token occurrence used by `explore word`. Deduplicated within
/// `(file, line)` but the same token can repeat across different lines.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
pub struct Identifier {
    pub token: String,
    pub line: u32,
}

/// A call-/type-/macro-reference captured from source. `caller_symbol` is the
/// enclosing function/method name when we could determine it from the parse
/// tree; otherwise None (free-floating code, module-level expressions, etc.).
///
/// Callsite extraction is intentionally name-based and noisy — we record the
/// leaf identifier of the call target (e.g. `c` for `a.b.c()`) without trying
/// to resolve overloads or trait dispatch. The `impact` command exposes the
/// resulting candidate list and leaves disambiguation to the agent/user.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Callsite {
    pub caller_line: u32,
    pub caller_symbol: Option<String>,
    pub callee_name: String,
    pub callee_kind: String, // "call" | "type_ref" | "macro"
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ParsedFile {
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
    pub identifiers: Vec<Identifier>,
    pub callsites: Vec<Callsite>,
}

/// Parse `source` and extract symbols + imports + identifiers for `language`.
/// Failures inside the language-specific extractors are demoted to empty
/// results — a malformed query or surprising tree shouldn't abort indexing.
pub fn parse_file(language: Language, source: &str) -> Result<ParsedFile> {
    let Some(ts_lang) = language.ts_language() else {
        // Best-effort regex fallback for unknown languages; used by the
        // `outline` command when the file has no recognized extension.
        return Ok(fallback_outline(source));
    };
    let mut parser = Parser::new();
    parser
        .set_language(&ts_lang)
        .with_context(|| format!("setting tree-sitter language for `{}`", language.as_str()))?;
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Ok(ParsedFile::default()),
    };

    let mut out = ParsedFile::default();
    match language {
        Language::Rust => extract_rust(&tree, &ts_lang, source, &mut out),
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            extract_jsts(&tree, &ts_lang, source, &mut out)
        }
        Language::Python => extract_python(&tree, &ts_lang, source, &mut out),
        Language::Go => extract_go(&tree, &ts_lang, source, &mut out),
        Language::C | Language::Cpp => extract_c_like(&tree, &ts_lang, source, &mut out, language),
        Language::Unknown => {}
    }

    out.identifiers = extract_identifiers(tree.root_node(), source);
    out.callsites = extract_callsites(language, tree.root_node(), source);
    Ok(out)
}

/// Very simple regex-style outline for files whose language we don't parse.
/// Used by `explore outline` as a fallback so the command still produces
/// *something* useful instead of `not supported`.
pub fn fallback_outline(source: &str) -> ParsedFile {
    let mut out = ParsedFile::default();
    for (i, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        for (prefix, kind) in [
            ("fn ", "function"),
            ("func ", "function"),
            ("def ", "function"),
            ("class ", "class"),
            ("struct ", "struct"),
            ("type ", "type"),
        ] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    out.symbols.push(Symbol {
                        name,
                        kind: kind.to_string(),
                        line: (i as u32) + 1,
                        end_line: None,
                        parent: None,
                        visibility: None,
                        signature: Some(trimmed_signature(line)),
                    });
                    break;
                }
            }
        }
    }
    out
}

// ---------- Rust ----------

fn extract_rust(
    tree: &tree_sitter::Tree,
    lang: &tree_sitter::Language,
    src: &str,
    out: &mut ParsedFile,
) {
    // Type/module-shaped items. Functions are handled separately so we can
    // exclude impl-block methods (those are added by the methods query below
    // with `parent` set to the impl type).
    let q = r#"
        (struct_item name: (type_identifier) @name) @def
        (enum_item name: (type_identifier) @name) @def
        (trait_item name: (type_identifier) @name) @def
        (mod_item name: (identifier) @name) @def
        (type_item name: (type_identifier) @name) @def
        (const_item name: (identifier) @name) @def
        (static_item name: (identifier) @name) @def
    "#;
    run_query_kinds(lang, src, tree, q, out, |kind_node, _| {
        Some(match kind_node.kind() {
            "struct_item" => ("struct", None),
            "enum_item" => ("enum", None),
            "trait_item" => ("trait", None),
            "mod_item" => ("module", None),
            "type_item" => ("type", None),
            "const_item" => ("const", None),
            "static_item" => ("static", None),
            _ => return None,
        })
    });

    // Free-standing functions: every `function_item` whose nearest enclosing
    // item is NOT an `impl_item`. Methods inside `impl` blocks are emitted by
    // the dedicated query below with `parent` set; emitting them here too
    // would produce duplicate rows where the wrong one (no parent) is the
    // earlier insert.
    let q_fn = "(function_item name: (identifier) @name) @def";
    if let Ok(query) = Query::new(lang, q_fn) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            let mut name_node: Option<Node> = None;
            let mut def_node: Option<Node> = None;
            for cap in m.captures {
                let cn = &query.capture_names()[cap.index as usize];
                match *cn {
                    "name" => name_node = Some(cap.node),
                    "def" => def_node = Some(cap.node),
                    _ => {}
                }
            }
            if let (Some(n), Some(d)) = (name_node, def_node) {
                if has_ancestor_kind(d, "impl_item") {
                    continue;
                }
                out.symbols.push(Symbol {
                    name: node_text(n, src).to_string(),
                    kind: "function".to_string(),
                    line: (d.start_position().row as u32) + 1,
                    end_line: Some((d.end_position().row as u32) + 1),
                    parent: None,
                    visibility: rust_visibility(d, src),
                    signature: Some(first_line_signature(src, d)),
                });
            }
        }
    }

    // Methods inside `impl` blocks — extracted separately so we can capture
    // the receiver/trait type as `parent`.
    let q_methods = r#"
        (impl_item
            type: (_) @impl_type
            body: (declaration_list
                (function_item name: (identifier) @method_name) @method_def))
    "#;
    if let Ok(query) = Query::new(lang, q_methods) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            let mut impl_type: Option<&str> = None;
            let mut method_name: Option<&str> = None;
            let mut method_def: Option<Node> = None;
            for cap in m.captures {
                let name = &query.capture_names()[cap.index as usize];
                let text = node_text(cap.node, src);
                match *name {
                    "impl_type" => impl_type = Some(text),
                    "method_name" => method_name = Some(text),
                    "method_def" => method_def = Some(cap.node),
                    _ => {}
                }
            }
            if let (Some(name), Some(def)) = (method_name, method_def) {
                let visibility = rust_visibility(def, src);
                out.symbols.push(Symbol {
                    name: name.to_string(),
                    kind: "method".to_string(),
                    line: (def.start_position().row as u32) + 1,
                    end_line: Some((def.end_position().row as u32) + 1),
                    parent: impl_type.map(|s| s.to_string()),
                    visibility,
                    signature: Some(first_line_signature(src, def)),
                });
            }
        }
    }

    // Imports: `use` declarations (the entire `use ...;` text is captured).
    let q_use = "(use_declaration) @use";
    if let Ok(query) = Query::new(lang, q_use) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            for cap in m.captures {
                let text = node_text(cap.node, src);
                let cleaned = text
                    .trim_start_matches("pub ")
                    .trim_start_matches("pub(crate) ")
                    .trim_start_matches("pub(super) ")
                    .trim_start_matches("use ")
                    .trim_end_matches(';')
                    .trim()
                    .to_string();
                out.imports.push(Import {
                    target: cleaned,
                    line: (cap.node.start_position().row as u32) + 1,
                });
            }
        }
    }

    // Fill in visibility / end_line / signature for the non-method symbols.
    fill_rust_visibility(tree.root_node(), src, out);
}

fn rust_visibility(node: Node, src: &str) -> Option<String> {
    // Walk previous siblings to find a `visibility_modifier`. In tree-sitter-rust
    // the visibility modifier is a *child* of the item node, so look there first.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return Some(node_text(child, src).to_string());
        }
    }
    None
}

fn fill_rust_visibility(root: Node, src: &str, out: &mut ParsedFile) {
    let lines: Vec<&str> = src.lines().collect();
    let mut by_line: std::collections::HashMap<u32, Node> = std::collections::HashMap::new();
    visit(root, &mut |n| match n.kind() {
        "function_item" | "struct_item" | "enum_item" | "trait_item" | "mod_item" | "type_item"
        | "const_item" | "static_item" => {
            by_line.insert((n.start_position().row as u32) + 1, n);
        }
        _ => {}
    });
    for sym in out.symbols.iter_mut() {
        if sym.kind == "method" {
            // already populated
            continue;
        }
        if let Some(n) = by_line.get(&sym.line) {
            sym.visibility = rust_visibility(*n, src);
            sym.end_line = Some((n.end_position().row as u32) + 1);
            if sym.signature.is_none() {
                sym.signature = lines
                    .get(sym.line.saturating_sub(1) as usize)
                    .map(|l| trimmed_signature(l));
            }
        }
    }
}

// ---------- TypeScript / TSX / JavaScript ----------

fn extract_jsts(
    tree: &tree_sitter::Tree,
    lang: &tree_sitter::Language,
    src: &str,
    out: &mut ParsedFile,
) {
    // The class-name node kind differs between JS (`identifier`) and TS
    // (`type_identifier`), so try each separately — invalid queries return
    // `Err` and are silently skipped, which would have eaten the entire block
    // if combined.
    let queries = [
        "(function_declaration name: (identifier) @name) @def",
        "(class_declaration name: (type_identifier) @name) @def",
        "(class_declaration name: (identifier) @name) @def",
        "(interface_declaration name: (type_identifier) @name) @def",
        "(type_alias_declaration name: (type_identifier) @name) @def",
        "(lexical_declaration (variable_declarator name: (identifier) @name)) @def",
        "(variable_declaration (variable_declarator name: (identifier) @name)) @def",
    ];
    for pat in queries {
        run_query_kinds(lang, src, tree, pat, out, |def_node, _name_node| {
            let k = def_node.kind();
            Some(match k {
                "function_declaration" => ("function", None),
                "class_declaration" => ("class", None),
                "interface_declaration" => ("interface", None),
                "type_alias_declaration" => ("type", None),
                "lexical_declaration" | "variable_declaration" => ("const", None),
                _ => return None,
            })
        });
    }

    // Methods inside classes (sets parent = class name).
    let q_methods = r#"
        (class_declaration
            name: (_) @class_name
            body: (class_body
                (method_definition name: (property_identifier) @method_name) @method_def))
    "#;
    if let Ok(query) = Query::new(lang, q_methods) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            let mut cls: Option<&str> = None;
            let mut mname: Option<&str> = None;
            let mut mdef: Option<Node> = None;
            for cap in m.captures {
                let name = &query.capture_names()[cap.index as usize];
                let text = node_text(cap.node, src);
                match *name {
                    "class_name" => cls = Some(text),
                    "method_name" => mname = Some(text),
                    "method_def" => mdef = Some(cap.node),
                    _ => {}
                }
            }
            if let (Some(n), Some(def)) = (mname, mdef) {
                out.symbols.push(Symbol {
                    name: n.to_string(),
                    kind: "method".to_string(),
                    line: (def.start_position().row as u32) + 1,
                    end_line: Some((def.end_position().row as u32) + 1),
                    parent: cls.map(|s| s.to_string()),
                    visibility: None,
                    signature: Some(first_line_signature(src, def)),
                });
            }
        }
    }

    // Imports + exports.
    let q_import = r#"
        (import_statement) @imp
        (export_statement) @imp
    "#;
    if let Ok(query) = Query::new(lang, q_import) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            for cap in m.captures {
                let text = node_text(cap.node, src);
                let one_line = text.lines().next().unwrap_or(text).trim().to_string();
                out.imports.push(Import {
                    target: one_line,
                    line: (cap.node.start_position().row as u32) + 1,
                });
            }
        }
    }
}

// ---------- Python ----------

fn extract_python(
    tree: &tree_sitter::Tree,
    lang: &tree_sitter::Language,
    src: &str,
    out: &mut ParsedFile,
) {
    // Free-standing functions and classes at the top level (we want class
    // methods captured separately so parent= class name).
    let q = r#"
        (module
            (function_definition name: (identifier) @name) @def)
        (module
            (class_definition name: (identifier) @name) @def)
        (module
            (decorated_definition
                definition: (function_definition name: (identifier) @name) @def))
        (module
            (decorated_definition
                definition: (class_definition name: (identifier) @name) @def))
    "#;
    run_query_kinds(lang, src, tree, q, out, |def_node, _name_node| {
        Some(match def_node.kind() {
            "function_definition" => ("function", None),
            "class_definition" => ("class", None),
            _ => return None,
        })
    });

    // Methods (function_definition inside class_definition body).
    let q_methods = r#"
        (class_definition
            name: (identifier) @class_name
            body: (block
                (function_definition name: (identifier) @method_name) @method_def))
        (class_definition
            name: (identifier) @class_name
            body: (block
                (decorated_definition
                    definition: (function_definition name: (identifier) @method_name) @method_def)))
    "#;
    if let Ok(query) = Query::new(lang, q_methods) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            let mut cls: Option<&str> = None;
            let mut mname: Option<&str> = None;
            let mut mdef: Option<Node> = None;
            for cap in m.captures {
                let name = &query.capture_names()[cap.index as usize];
                let text = node_text(cap.node, src);
                match *name {
                    "class_name" => cls = Some(text),
                    "method_name" => mname = Some(text),
                    "method_def" => mdef = Some(cap.node),
                    _ => {}
                }
            }
            if let (Some(n), Some(def)) = (mname, mdef) {
                out.symbols.push(Symbol {
                    name: n.to_string(),
                    kind: "method".to_string(),
                    line: (def.start_position().row as u32) + 1,
                    end_line: Some((def.end_position().row as u32) + 1),
                    parent: cls.map(|s| s.to_string()),
                    visibility: None,
                    signature: Some(first_line_signature(src, def)),
                });
            }
        }
    }

    // Top-level UPPER_CASE constants (best-effort).
    let q_const = r#"
        (module
            (expression_statement
                (assignment left: (identifier) @name)) @def)
    "#;
    if let Ok(query) = Query::new(lang, q_const) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            let mut nname: Option<&str> = None;
            let mut ndef: Option<Node> = None;
            for cap in m.captures {
                let name = &query.capture_names()[cap.index as usize];
                let text = node_text(cap.node, src);
                match *name {
                    "name" => nname = Some(text),
                    "def" => ndef = Some(cap.node),
                    _ => {}
                }
            }
            if let (Some(n), Some(def)) = (nname, ndef)
                && n.chars()
                    .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
            {
                out.symbols.push(Symbol {
                    name: n.to_string(),
                    kind: "const".to_string(),
                    line: (def.start_position().row as u32) + 1,
                    end_line: None,
                    parent: None,
                    visibility: None,
                    signature: Some(first_line_signature(src, def)),
                });
            }
        }
    }

    // Imports.
    let q_import = r#"
        (import_statement) @imp
        (import_from_statement) @imp
    "#;
    if let Ok(query) = Query::new(lang, q_import) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            for cap in m.captures {
                out.imports.push(Import {
                    target: node_text(cap.node, src).trim().to_string(),
                    line: (cap.node.start_position().row as u32) + 1,
                });
            }
        }
    }
}

// ---------- Go ----------

fn extract_go(
    tree: &tree_sitter::Tree,
    lang: &tree_sitter::Language,
    src: &str,
    out: &mut ParsedFile,
) {
    let q = r#"
        (function_declaration name: (identifier) @name) @def
        (type_spec name: (type_identifier) @name) @def
        (const_spec name: (identifier) @name) @def
        (var_spec name: (identifier) @name) @def
    "#;
    run_query_kinds(lang, src, tree, q, out, |def, _| {
        Some(match def.kind() {
            "function_declaration" => ("function", None),
            "type_spec" => ("type", None),
            "const_spec" => ("const", None),
            "var_spec" => ("var", None),
            _ => return None,
        })
    });

    // Methods: function with receiver. Capture the receiver type as parent.
    let q_methods = r#"
        (method_declaration
            receiver: (parameter_list
                (parameter_declaration
                    type: [(pointer_type (type_identifier) @recv) (type_identifier) @recv]))
            name: (field_identifier) @method_name) @method_def
    "#;
    if let Ok(query) = Query::new(lang, q_methods) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            let mut recv: Option<&str> = None;
            let mut name: Option<&str> = None;
            let mut def: Option<Node> = None;
            for cap in m.captures {
                let cname = &query.capture_names()[cap.index as usize];
                let text = node_text(cap.node, src);
                match *cname {
                    "recv" => recv = Some(text),
                    "method_name" => name = Some(text),
                    "method_def" => def = Some(cap.node),
                    _ => {}
                }
            }
            if let (Some(n), Some(d)) = (name, def) {
                out.symbols.push(Symbol {
                    name: n.to_string(),
                    kind: "method".to_string(),
                    line: (d.start_position().row as u32) + 1,
                    end_line: Some((d.end_position().row as u32) + 1),
                    parent: recv.map(|s| s.to_string()),
                    visibility: None,
                    signature: Some(first_line_signature(src, d)),
                });
            }
        }
    }

    let q_import = "(import_declaration) @imp";
    if let Ok(query) = Query::new(lang, q_import) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            for cap in m.captures {
                // Multi-line `import (...)` blocks: store the first-line head;
                // each individual spec also lives in the tree as `import_spec`.
                out.imports.push(Import {
                    target: node_text(cap.node, src)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string(),
                    line: (cap.node.start_position().row as u32) + 1,
                });
            }
        }
    }

    // Individual import_spec entries (the "github.com/x/y" string).
    let q_import_spec = "(import_spec path: (interpreted_string_literal) @path)";
    if let Ok(query) = Query::new(lang, q_import_spec) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            for cap in m.captures {
                out.imports.push(Import {
                    target: node_text(cap.node, src).trim_matches('"').to_string(),
                    line: (cap.node.start_position().row as u32) + 1,
                });
            }
        }
    }
}

// ---------- C / C++ ----------

fn extract_c_like(
    tree: &tree_sitter::Tree,
    lang: &tree_sitter::Language,
    src: &str,
    out: &mut ParsedFile,
    which: Language,
) {
    // Function definitions (both C and C++).
    let q_fn = r#"
        (function_definition
            declarator: (function_declarator
                declarator: [(identifier) @name (field_identifier) @name (qualified_identifier) @name])) @def
    "#;
    if let Ok(query) = Query::new(lang, q_fn) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            let mut name: Option<&str> = None;
            let mut def: Option<Node> = None;
            for cap in m.captures {
                let cn = &query.capture_names()[cap.index as usize];
                let text = node_text(cap.node, src);
                match *cn {
                    "name" => name = Some(text),
                    "def" => def = Some(cap.node),
                    _ => {}
                }
            }
            if let (Some(n), Some(d)) = (name, def) {
                out.symbols.push(Symbol {
                    name: n.to_string(),
                    kind: "function".to_string(),
                    line: (d.start_position().row as u32) + 1,
                    end_line: Some((d.end_position().row as u32) + 1),
                    parent: None,
                    visibility: None,
                    signature: Some(first_line_signature(src, d)),
                });
            }
        }
    }

    // Type-like declarations.
    let q_types = r#"
        (struct_specifier name: (type_identifier) @name) @def
        (enum_specifier name: (type_identifier) @name) @def
    "#;
    run_query_kinds(lang, src, tree, q_types, out, |def, _| {
        Some(match def.kind() {
            "struct_specifier" => ("struct", None),
            "enum_specifier" => ("enum", None),
            _ => return None,
        })
    });

    if which == Language::Cpp {
        let q_cpp = r#"
            (class_specifier name: (type_identifier) @name) @def
            (namespace_definition name: (namespace_identifier) @name) @def
        "#;
        run_query_kinds(lang, src, tree, q_cpp, out, |def, _| {
            Some(match def.kind() {
                "class_specifier" => ("class", None),
                "namespace_definition" => ("module", None),
                _ => return None,
            })
        });
    }

    // #include directives.
    let q_inc = "(preproc_include path: (_) @path) @inc";
    if let Ok(query) = Query::new(lang, q_inc) {
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            for cap in m.captures {
                let cn = &query.capture_names()[cap.index as usize];
                if *cn == "path" {
                    out.imports.push(Import {
                        target: node_text(cap.node, src)
                            .trim_matches(|c| c == '<' || c == '>' || c == '"')
                            .to_string(),
                        line: (cap.node.start_position().row as u32) + 1,
                    });
                }
            }
        }
    }
}

// ---------- Identifier scan (all languages) ----------

fn extract_identifiers(root: Node, src: &str) -> Vec<Identifier> {
    let mut out: Vec<Identifier> = Vec::new();
    let mut seen: HashSet<(String, u32)> = HashSet::new();
    visit(root, &mut |n| match n.kind() {
        "identifier"
        | "type_identifier"
        | "field_identifier"
        | "property_identifier"
        | "namespace_identifier"
        | "scoped_identifier"
        | "shorthand_property_identifier"
        | "shorthand_property_identifier_pattern" => {
            let text = node_text(n, src);
            if text.len() < 2 || text.chars().all(|c| c.is_ascii_digit()) {
                return;
            }
            let line = (n.start_position().row as u32) + 1;
            let key = (text.to_string(), line);
            if seen.insert(key.clone()) {
                out.push(Identifier { token: key.0, line });
            }
        }
        _ => {}
    });
    out
}

// ---------- Callsite scan ----------

/// Walk the tree gathering callsites + type refs + macro invocations. The
/// extraction is name-based (leaf identifier of the call target) and
/// deliberately noisy — `impact` surfaces candidate sites and leaves
/// disambiguation to the caller.
fn extract_callsites<'a>(language: Language, root: Node<'a>, src: &str) -> Vec<Callsite> {
    let mut out: Vec<Callsite> = Vec::new();
    visit(root, &mut |n| {
        match n.kind() {
            // Function/method calls (all languages share this kind name in
            // tree-sitter; Python uses "call", others "call_expression").
            "call_expression" | "call" => {
                if let Some(name) = call_target_leaf(n, src) {
                    out.push(Callsite {
                        caller_line: (n.start_position().row as u32) + 1,
                        caller_symbol: enclosing_symbol_name(n, src, language),
                        callee_name: name,
                        callee_kind: "call".to_string(),
                    });
                }
            }
            // `new Foo(...)` in TS/JS, Go composite literals, C++ new
            "new_expression" => {
                if let Some(name) = new_target_leaf(n, src) {
                    out.push(Callsite {
                        caller_line: (n.start_position().row as u32) + 1,
                        caller_symbol: enclosing_symbol_name(n, src, language),
                        callee_name: name,
                        callee_kind: "call".to_string(),
                    });
                }
            }
            // Rust macros: `println!`, `vec!`, custom!.
            "macro_invocation" => {
                if let Some(name) = macro_name(n, src) {
                    out.push(Callsite {
                        caller_line: (n.start_position().row as u32) + 1,
                        caller_symbol: enclosing_symbol_name(n, src, language),
                        callee_name: name,
                        callee_kind: "macro".to_string(),
                    });
                }
            }
            // Go composite literal: `Foo{...}` — the type is recorded.
            "composite_literal" => {
                if let Some(t) = composite_literal_type(n, src) {
                    out.push(Callsite {
                        caller_line: (n.start_position().row as u32) + 1,
                        caller_symbol: enclosing_symbol_name(n, src, language),
                        callee_name: t,
                        callee_kind: "type_ref".to_string(),
                    });
                }
            }
            // Type references (Rust, TS, C/C++) appearing in type positions:
            // struct fields, fn signatures, generics, return types. We use
            // the `type_identifier` node kind directly — it's both common
            // across these grammars and reliably *in* a type position.
            "type_identifier" => {
                // Skip if this is the *defining* name of a type/trait/struct
                // (already captured as a Symbol). Combined predicate to keep
                // the match arm body shallow.
                let text = node_text(n, src);
                if !is_defining_type_name(n) && text.len() >= 2 {
                    out.push(Callsite {
                        caller_line: (n.start_position().row as u32) + 1,
                        caller_symbol: enclosing_symbol_name(n, src, language),
                        callee_name: text.to_string(),
                        callee_kind: "type_ref".to_string(),
                    });
                }
            }
            _ => {}
        }
    });
    out
}

/// Best-effort: pull the leaf identifier of a call expression's "function"
/// field — for `a.b.c()` that's `c`; for `foo()` it's `foo`; for `(expr)()`
/// we give up.
fn call_target_leaf<'a>(node: Node<'a>, src: &str) -> Option<String> {
    let func = node
        .child_by_field_name("function")
        .or_else(|| node.child_by_field_name("callee"))
        .or_else(|| node.child(0))?;
    Some(rightmost_identifier(func, src))
}

fn new_target_leaf<'a>(node: Node<'a>, src: &str) -> Option<String> {
    // TS/JS uses `constructor`, but the underlying child is typically the
    // first non-keyword child. Walk children until we find an identifier-ish
    // node.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if matches!(
            k,
            "identifier"
                | "type_identifier"
                | "member_expression"
                | "scoped_identifier"
                | "qualified_identifier"
        ) {
            return Some(rightmost_identifier(child, src));
        }
    }
    None
}

fn macro_name<'a>(node: Node<'a>, src: &str) -> Option<String> {
    // tree-sitter-rust: macro_invocation -> macro (identifier or scoped_identifier)
    if let Some(macro_node) = node.child_by_field_name("macro") {
        return Some(rightmost_identifier(macro_node, src));
    }
    Some(rightmost_identifier(node.child(0)?, src))
}

fn composite_literal_type<'a>(node: Node<'a>, src: &str) -> Option<String> {
    let t = node.child_by_field_name("type")?;
    Some(rightmost_identifier(t, src))
}

/// Walk a (possibly compound) name expression to its rightmost identifier
/// leaf. For `a.b.c` → `c`; for `crate::foo::Bar` → `Bar`; for plain
/// `foo` → `foo`. Falls back to the whole text if we can't find a leaf.
fn rightmost_identifier<'a>(node: Node<'a>, src: &str) -> String {
    match node.kind() {
        "identifier"
        | "type_identifier"
        | "field_identifier"
        | "property_identifier"
        | "namespace_identifier" => node_text(node, src).to_string(),
        _ => {
            // Walk last child down until we find a leaf identifier.
            let mut cur = node;
            loop {
                let n = cur.child_count();
                if n == 0 {
                    return node_text(cur, src).to_string();
                }
                let last = cur.child(n - 1).unwrap();
                if matches!(
                    last.kind(),
                    "identifier"
                        | "type_identifier"
                        | "field_identifier"
                        | "property_identifier"
                        | "namespace_identifier"
                ) {
                    return node_text(last, src).to_string();
                }
                // Avoid infinite loop on weird trees.
                if last.id() == cur.id() {
                    return node_text(cur, src).to_string();
                }
                cur = last;
            }
        }
    }
}

/// Heuristic: when a `type_identifier` is the immediate name child of a
/// declaration (struct/enum/trait/interface/class/type-alias), it's the
/// *defining* name and is already in `outline_symbols` — exclude it from
/// callsites. We detect this by inspecting the parent node kind.
fn is_defining_type_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(
        parent.kind(),
        "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "union_item"
            | "type_alias_declaration"
            | "interface_declaration"
            | "class_declaration"
            | "class_specifier"
            | "struct_specifier"
            | "enum_specifier"
            | "type_spec"
            | "type_declaration"
    )
}

/// Walk up the parents of `node` looking for the nearest enclosing function
/// or method declaration and return its name (if it has one).
fn enclosing_symbol_name<'a>(node: Node<'a>, src: &str, language: Language) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        let kind = n.kind();
        let is_callable = match language {
            Language::Rust => matches!(kind, "function_item"),
            Language::TypeScript | Language::Tsx | Language::JavaScript => matches!(
                kind,
                "function_declaration"
                    | "method_definition"
                    | "arrow_function"
                    | "function_expression"
            ),
            Language::Python => matches!(kind, "function_definition"),
            Language::Go => matches!(kind, "function_declaration" | "method_declaration"),
            Language::C | Language::Cpp => matches!(kind, "function_definition"),
            Language::Unknown => false,
        };
        if is_callable && let Some(name_node) = n.child_by_field_name("name") {
            return Some(node_text(name_node, src).to_string());
        }
        cur = n.parent();
    }
    None
}

// ---------- Helpers ----------

fn visit<'a, F: FnMut(Node<'a>)>(node: Node<'a>, f: &mut F) {
    f(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, f);
    }
}

/// Walk parent links looking for an ancestor whose `kind()` matches.
fn has_ancestor_kind(node: Node<'_>, kind: &str) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == kind {
            return true;
        }
        cur = n.parent();
    }
    false
}

fn node_text<'a>(node: Node<'a>, src: &'a str) -> &'a str {
    &src[node.byte_range()]
}

fn first_line_signature(src: &str, node: Node) -> String {
    let start = node.start_position().row;
    src.lines()
        .nth(start)
        .map(trimmed_signature)
        .unwrap_or_default()
}

fn trimmed_signature(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= 200 {
        trimmed.to_string()
    } else {
        let mut s: String = trimmed.chars().take(197).collect();
        s.push_str("...");
        s
    }
}

/// Run a tree-sitter `Query` whose patterns each carry `@name` (the symbol name)
/// and `@def` (the enclosing definition node), and translate kind via `kind_of`.
fn run_query_kinds<F>(
    lang: &tree_sitter::Language,
    src: &str,
    tree: &tree_sitter::Tree,
    pattern: &str,
    out: &mut ParsedFile,
    mut kind_of: F,
) where
    F: FnMut(Node, Node) -> Option<(&'static str, Option<String>)>,
{
    let query = match Query::new(lang, pattern) {
        Ok(q) => q,
        Err(_) => return,
    };
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
    while let Some(m) = it.next() {
        let mut name_node: Option<Node> = None;
        let mut def_node: Option<Node> = None;
        for cap in m.captures {
            let cn = &query.capture_names()[cap.index as usize];
            match *cn {
                "name" => name_node = Some(cap.node),
                "def" => def_node = Some(cap.node),
                _ => {}
            }
        }
        if let (Some(n), Some(d)) = (name_node, def_node)
            && let Some((kind, vis)) = kind_of(d, n)
        {
            let name = node_text(n, src).to_string();
            out.symbols.push(Symbol {
                name,
                kind: kind.to_string(),
                line: (d.start_position().row as u32) + 1,
                end_line: Some((d.end_position().row as u32) + 1),
                parent: None,
                visibility: vis,
                signature: Some(first_line_signature(src, d)),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(parsed: &ParsedFile) -> Vec<(&str, &str)> {
        parsed
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str()))
            .collect()
    }

    #[test]
    fn rust_basic_symbols() {
        let src = r#"
pub fn hello() {}
struct Foo { x: i32 }
trait Greet { fn greet(&self); }
impl Foo {
    pub fn bar(&self) {}
    fn baz(&self) {}
}
pub use std::path::Path;
use std::collections::HashMap;
const N: u32 = 1;
"#;
        let p = parse_file(Language::Rust, src).unwrap();
        let n = names(&p);
        assert!(
            n.iter()
                .any(|(name, k)| *name == "hello" && *k == "function")
        );
        assert!(n.iter().any(|(name, k)| *name == "Foo" && *k == "struct"));
        assert!(n.iter().any(|(name, k)| *name == "Greet" && *k == "trait"));
        assert!(n.iter().any(|(name, k)| *name == "bar" && *k == "method"));
        assert!(n.iter().any(|(name, k)| *name == "N" && *k == "const"));
        // visibility detected
        let hello = p.symbols.iter().find(|s| s.name == "hello").unwrap();
        assert_eq!(hello.visibility.as_deref(), Some("pub"));
        // imports
        assert!(p.imports.iter().any(|i| i.target.contains("HashMap")));
        assert!(p.imports.iter().any(|i| i.target.contains("Path")));
        // methods carry parent = impl type
        let bar = p.symbols.iter().find(|s| s.name == "bar").unwrap();
        assert_eq!(bar.parent.as_deref(), Some("Foo"));
    }

    #[test]
    fn typescript_basic_symbols() {
        let src = r#"
import { foo } from "bar";
export function hello(): void {}
class Greeter {
    greet(): string { return "hi"; }
}
interface Box { x: number }
"#;
        let p = parse_file(Language::TypeScript, src).unwrap();
        let n = names(&p);
        assert!(
            n.iter()
                .any(|(name, k)| *name == "hello" && *k == "function")
        );
        assert!(
            n.iter()
                .any(|(name, k)| *name == "Greeter" && *k == "class")
        );
        assert!(n.iter().any(|(name, k)| *name == "greet" && *k == "method"));
        assert!(
            n.iter()
                .any(|(name, k)| *name == "Box" && *k == "interface")
        );
        assert!(p.imports.iter().any(|i| i.target.contains("from \"bar\"")));
    }

    #[test]
    fn python_basic_symbols() {
        let src = r#"
import os
from typing import List

CONSTANT = 1

def free_fn():
    pass

class Animal:
    def speak(self):
        pass
"#;
        let p = parse_file(Language::Python, src).unwrap();
        let n = names(&p);
        assert!(
            n.iter()
                .any(|(name, k)| *name == "free_fn" && *k == "function")
        );
        assert!(n.iter().any(|(name, k)| *name == "Animal" && *k == "class"));
        assert!(n.iter().any(|(name, k)| *name == "speak" && *k == "method"));
        assert!(
            n.iter()
                .any(|(name, k)| *name == "CONSTANT" && *k == "const")
        );
        assert!(p.imports.iter().any(|i| i.target.contains("import os")));
        assert!(p.imports.iter().any(|i| i.target.contains("from typing")));
        let speak = p.symbols.iter().find(|s| s.name == "speak").unwrap();
        assert_eq!(speak.parent.as_deref(), Some("Animal"));
    }

    #[test]
    fn go_basic_symbols() {
        let src = r#"
package main

import "fmt"

type Greeter struct{}

func (g *Greeter) Hello() string {
    return "hi"
}

func main() {
    fmt.Println("hi")
}
"#;
        let p = parse_file(Language::Go, src).unwrap();
        let n = names(&p);
        assert!(
            n.iter()
                .any(|(name, k)| *name == "main" && *k == "function")
        );
        assert!(n.iter().any(|(name, k)| *name == "Greeter" && *k == "type"));
        assert!(n.iter().any(|(name, k)| *name == "Hello" && *k == "method"));
        // import string captured.
        assert!(p.imports.iter().any(|i| i.target.contains("fmt")));
        // method parent is receiver type
        let hello = p.symbols.iter().find(|s| s.name == "Hello").unwrap();
        assert_eq!(hello.parent.as_deref(), Some("Greeter"));
    }

    #[test]
    fn identifiers_collected_and_short_tokens_filtered() {
        let src = "fn hello() { let x = 1; let abc = 2; }";
        let p = parse_file(Language::Rust, src).unwrap();
        assert!(p.identifiers.iter().any(|i| i.token == "hello"));
        assert!(p.identifiers.iter().any(|i| i.token == "abc"));
        // 1-char identifiers filtered
        assert!(!p.identifiers.iter().any(|i| i.token == "x"));
    }

    #[test]
    fn rust_callsites_extracted_with_enclosing_function() {
        let src = r#"
fn outer() {
    inner();
    println!("hi");
    let _: Foo = Foo {};
}
struct Foo;
fn inner() {}
"#;
        let p = parse_file(Language::Rust, src).unwrap();
        // We expect `inner` as a call, `println` as a macro, and `Foo` as a
        // type_ref (the field-less struct literal). The struct *definition*
        // for `Foo` must not be in the callsites list.
        let has_call_inner = p
            .callsites
            .iter()
            .any(|c| c.callee_name == "inner" && c.callee_kind == "call");
        assert!(has_call_inner, "expected a call to `inner`");
        let has_macro_println = p
            .callsites
            .iter()
            .any(|c| c.callee_name == "println" && c.callee_kind == "macro");
        assert!(has_macro_println, "expected `println` macro");
        let inner_calls_outer = p
            .callsites
            .iter()
            .filter(|c| c.callee_name == "inner")
            .all(|c| c.caller_symbol.as_deref() == Some("outer"));
        assert!(
            inner_calls_outer,
            "`inner` call should be inside `outer`'s body"
        );
        // The `Foo` defining name (line of `struct Foo;`) must not appear as
        // a type_ref callsite.
        let foo_def_line = src
            .lines()
            .enumerate()
            .find(|(_, l)| l.trim_start().starts_with("struct Foo"))
            .map(|(i, _)| i + 1)
            .unwrap() as u32;
        assert!(
            !p.callsites.iter().any(|c| c.callee_name == "Foo"
                && c.callee_kind == "type_ref"
                && c.caller_line == foo_def_line),
            "the defining occurrence of `Foo` must not be a callsite"
        );
    }

    #[test]
    fn fallback_outline_handles_plain_text() {
        let src = "def python_like():\nfunc go_like()\nclass Thing:\n";
        let p = fallback_outline(src);
        assert!(p.symbols.iter().any(|s| s.name == "python_like"));
        assert!(p.symbols.iter().any(|s| s.name == "go_like"));
        assert!(p.symbols.iter().any(|s| s.name == "Thing"));
    }
}
