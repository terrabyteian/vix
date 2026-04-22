//! Tree-sitter highlighting for vix.
//!
//! Minimal surface: detect language from a path, parse the source, and emit
//! a flat list of byte-ranged highlight spans classified by a small set of
//! named scopes. The TUI crate owns the mapping from scope → color.

use std::ops::Range;
use std::path::Path;

use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// The set of scope names we recognize. Order must match `HIGHLIGHT_NAMES`
/// below so we can pass indices between tree-sitter and our code.
pub const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "function",
    "function.builtin",
    "function.macro",
    "function.method",
    "keyword",
    "keyword.control",
    "keyword.directive",
    "keyword.operator",
    "label",
    "namespace",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.escape",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Markdown,
    Json,
    Toml,
    Html,
    Css,
    Bash,
}

impl Language {
    pub fn from_path(path: &Path) -> Option<Language> {
        // Filename-first (e.g. `.bashrc`), then extension.
        if let Some(name @ (".bashrc" | ".bash_profile" | ".profile")) =
            path.file_name().and_then(|n| n.to_str())
        {
            let _ = name;
            return Some(Language::Bash);
        }
        let ext = path.extension()?.to_str()?;
        match ext {
            "rs" => Some(Language::Rust),
            "py" | "pyi" => Some(Language::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
            "ts" => Some(Language::TypeScript),
            "tsx" => Some(Language::Tsx),
            "go" => Some(Language::Go),
            "md" | "markdown" => Some(Language::Markdown),
            "json" => Some(Language::Json),
            "toml" => Some(Language::Toml),
            "html" | "htm" => Some(Language::Html),
            "css" => Some(Language::Css),
            "sh" | "bash" => Some(Language::Bash),
            _ => None,
        }
    }
}

/// A single highlight span: a byte range in the source plus a scope index
/// into [`HIGHLIGHT_NAMES`]. Spans do not overlap.
#[derive(Clone, Debug)]
pub struct HlSpan {
    pub range: Range<usize>,
    pub scope: usize,
}

/// A top-level definition surfaced by the symbol picker: its name, kind tag
/// (e.g. `"fn"`, `"struct"`), and byte offset where the name starts.
#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: &'static str,
    pub start_byte: usize,
}

/// Stateful highlighter. One per buffer. Currently re-parses from source on
/// each `highlight` call — incremental reparse is a later optimization.
pub struct SyntaxState {
    lang: Language,
    cfg: HighlightConfiguration,
    hl: Highlighter,
}

impl SyntaxState {
    pub fn new(lang: Language) -> anyhow::Result<Self> {
        let mut cfg = match lang {
            Language::Rust => HighlightConfiguration::new(
                tree_sitter_rust::LANGUAGE.into(),
                "rust",
                tree_sitter_rust::HIGHLIGHTS_QUERY,
                tree_sitter_rust::INJECTIONS_QUERY,
                "",
            )?,
            Language::Python => HighlightConfiguration::new(
                tree_sitter_python::LANGUAGE.into(),
                "python",
                tree_sitter_python::HIGHLIGHTS_QUERY,
                "",
                "",
            )?,
            Language::JavaScript => HighlightConfiguration::new(
                tree_sitter_javascript::LANGUAGE.into(),
                "javascript",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::INJECTIONS_QUERY,
                tree_sitter_javascript::LOCALS_QUERY,
            )?,
            Language::TypeScript => HighlightConfiguration::new(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "typescript",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                "",
                tree_sitter_typescript::LOCALS_QUERY,
            )?,
            Language::Tsx => HighlightConfiguration::new(
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                "tsx",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                "",
                tree_sitter_typescript::LOCALS_QUERY,
            )?,
            Language::Go => HighlightConfiguration::new(
                tree_sitter_go::LANGUAGE.into(),
                "go",
                tree_sitter_go::HIGHLIGHTS_QUERY,
                "",
                "",
            )?,
            Language::Markdown => HighlightConfiguration::new(
                tree_sitter_md::LANGUAGE.into(),
                "markdown",
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
                tree_sitter_md::INJECTION_QUERY_BLOCK,
                "",
            )?,
            Language::Json => HighlightConfiguration::new(
                tree_sitter_json::LANGUAGE.into(),
                "json",
                tree_sitter_json::HIGHLIGHTS_QUERY,
                "",
                "",
            )?,
            Language::Toml => HighlightConfiguration::new(
                tree_sitter_toml_ng::LANGUAGE.into(),
                "toml",
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
                "",
                "",
            )?,
            Language::Html => HighlightConfiguration::new(
                tree_sitter_html::LANGUAGE.into(),
                "html",
                tree_sitter_html::HIGHLIGHTS_QUERY,
                tree_sitter_html::INJECTIONS_QUERY,
                "",
            )?,
            Language::Css => HighlightConfiguration::new(
                tree_sitter_css::LANGUAGE.into(),
                "css",
                tree_sitter_css::HIGHLIGHTS_QUERY,
                "",
                "",
            )?,
            Language::Bash => HighlightConfiguration::new(
                tree_sitter_bash::LANGUAGE.into(),
                "bash",
                tree_sitter_bash::HIGHLIGHT_QUERY,
                "",
                "",
            )?,
        };
        cfg.configure(HIGHLIGHT_NAMES);
        Ok(Self {
            lang,
            cfg,
            hl: Highlighter::new(),
        })
    }

    pub fn language(&self) -> Language {
        self.lang
    }

    /// Extract top-level symbols from `source`. Returns an empty vec for
    /// languages without a symbol query registered.
    pub fn symbols(&self, source: &[u8]) -> anyhow::Result<Vec<Symbol>> {
        let Some(query_str) = symbol_query(self.lang) else {
            return Ok(Vec::new());
        };
        let ts_lang: tree_sitter::Language = match self.lang {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Language::Go => tree_sitter_go::LANGUAGE.into(),
            Language::Markdown => tree_sitter_md::LANGUAGE.into(),
            Language::Json => tree_sitter_json::LANGUAGE.into(),
            Language::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Language::Html => tree_sitter_html::LANGUAGE.into(),
            Language::Css => tree_sitter_css::LANGUAGE.into(),
            Language::Bash => tree_sitter_bash::LANGUAGE.into(),
        };
        let mut parser = Parser::new();
        parser.set_language(&ts_lang)?;
        let Some(tree) = parser.parse(source, None) else {
            return Ok(Vec::new());
        };
        let query = Query::new(&ts_lang, query_str)?;
        let capture_names = query.capture_names();
        let name_idx = capture_names.iter().position(|n| *n == "name");
        let Some(name_idx) = name_idx else { return Ok(Vec::new()) };
        let mut cursor = QueryCursor::new();
        let mut out: Vec<Symbol> = Vec::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            // Kind is the pattern index → map via symbol_kinds.
            let kind = symbol_kinds(self.lang)
                .get(m.pattern_index)
                .copied()
                .unwrap_or("sym");
            for cap in m.captures {
                if cap.index as usize != name_idx {
                    continue;
                }
                let node = cap.node;
                let start = node.start_byte();
                let end = node.end_byte();
                let name = std::str::from_utf8(&source[start..end])
                    .unwrap_or("?")
                    .to_string();
                out.push(Symbol {
                    name,
                    kind,
                    start_byte: start,
                });
            }
        }
        Ok(out)
    }

    /// Produce a flat span list for `source`. Nested highlights are flattened
    /// to the innermost scope covering each byte range.
    pub fn highlight(&mut self, source: &[u8]) -> anyhow::Result<Vec<HlSpan>> {
        let events = self.hl.highlight(&self.cfg, source, None, |_| None)?;
        let mut stack: Vec<usize> = Vec::new();
        let mut spans: Vec<HlSpan> = Vec::new();
        for event in events {
            match event? {
                HighlightEvent::HighlightStart(h) => stack.push(h.0),
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    if let Some(&scope) = stack.last() {
                        spans.push(HlSpan {
                            range: start..end,
                            scope,
                        });
                    }
                }
            }
        }
        Ok(spans)
    }
}

/// Per-language query string for the symbol picker. Each pattern must
/// capture exactly one `@name` node; the pattern's position in the query
/// corresponds to a kind tag in [`symbol_kinds`].
fn symbol_query(lang: Language) -> Option<&'static str> {
    Some(match lang {
        Language::Rust => RUST_SYMBOLS,
        Language::Python => PYTHON_SYMBOLS,
        Language::JavaScript => JS_SYMBOLS,
        Language::TypeScript | Language::Tsx => TS_SYMBOLS,
        Language::Go => GO_SYMBOLS,
        _ => return None,
    })
}

fn symbol_kinds(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::Rust => &["fn", "struct", "enum", "trait", "const", "static", "macro", "mod"],
        Language::Python => &["fn", "class"],
        Language::JavaScript => &["fn", "class", "method"],
        Language::TypeScript | Language::Tsx => &["fn", "class", "interface", "type", "method"],
        Language::Go => &["fn", "method", "type"],
        _ => &[],
    }
}

const RUST_SYMBOLS: &str = r#"
(function_item name: (identifier) @name)
(struct_item name: (type_identifier) @name)
(enum_item name: (type_identifier) @name)
(trait_item name: (type_identifier) @name)
(const_item name: (identifier) @name)
(static_item name: (identifier) @name)
(macro_definition name: (identifier) @name)
(mod_item name: (identifier) @name)
"#;

const PYTHON_SYMBOLS: &str = r#"
(function_definition name: (identifier) @name)
(class_definition name: (identifier) @name)
"#;

const JS_SYMBOLS: &str = r#"
(function_declaration name: (identifier) @name)
(class_declaration name: (identifier) @name)
(method_definition name: (property_identifier) @name)
"#;

const TS_SYMBOLS: &str = r#"
(function_declaration name: (identifier) @name)
(class_declaration name: (type_identifier) @name)
(interface_declaration name: (type_identifier) @name)
(type_alias_declaration name: (type_identifier) @name)
(method_definition name: (property_identifier) @name)
"#;

const GO_SYMBOLS: &str = r#"
(function_declaration name: (identifier) @name)
(method_declaration name: (field_identifier) @name)
(type_declaration (type_spec name: (type_identifier) @name))
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_from_path_rust() {
        assert_eq!(
            Language::from_path(Path::new("foo.rs")),
            Some(Language::Rust)
        );
        assert_eq!(Language::from_path(Path::new("foo.txt")), None);
    }

    #[test]
    fn language_from_path_extensions() {
        let cases = [
            ("a.py", Language::Python),
            ("a.js", Language::JavaScript),
            ("a.jsx", Language::JavaScript),
            ("a.ts", Language::TypeScript),
            ("a.tsx", Language::Tsx),
            ("a.go", Language::Go),
            ("a.md", Language::Markdown),
            ("a.json", Language::Json),
            ("a.toml", Language::Toml),
            ("a.html", Language::Html),
            ("a.css", Language::Css),
            ("a.sh", Language::Bash),
        ];
        for (p, want) in cases {
            assert_eq!(Language::from_path(Path::new(p)), Some(want), "path {p}");
        }
    }

    #[test]
    fn symbols_rust() {
        let s = SyntaxState::new(Language::Rust).unwrap();
        let src = b"fn hello() {}\nstruct Foo;\nenum Bar { A }\ntrait T {}\n";
        let syms = s.symbols(src).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello"), "got {names:?}");
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"Bar"));
        assert!(names.contains(&"T"));
        let kinds: Vec<&str> = syms.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&"fn"));
        assert!(kinds.contains(&"struct"));
        assert!(kinds.contains(&"enum"));
        assert!(kinds.contains(&"trait"));
    }

    #[test]
    fn symbols_python() {
        let s = SyntaxState::new(Language::Python).unwrap();
        let src = b"def foo():\n    pass\nclass Bar:\n    pass\n";
        let syms = s.symbols(src).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"Bar"));
    }

    #[test]
    fn symbols_unsupported_language_returns_empty() {
        let s = SyntaxState::new(Language::Json).unwrap();
        let syms = s.symbols(b"{}").unwrap();
        assert!(syms.is_empty());
    }

    #[test]
    fn all_languages_construct() {
        for lang in [
            Language::Rust,
            Language::Python,
            Language::JavaScript,
            Language::TypeScript,
            Language::Tsx,
            Language::Go,
            Language::Markdown,
            Language::Json,
            Language::Toml,
            Language::Html,
            Language::Css,
            Language::Bash,
        ] {
            SyntaxState::new(lang).unwrap_or_else(|e| panic!("{lang:?}: {e}"));
        }
    }

    #[test]
    fn highlight_rust_keyword() {
        let mut s = SyntaxState::new(Language::Rust).unwrap();
        let src = b"fn main() {}";
        let spans = s.highlight(src).unwrap();
        assert!(!spans.is_empty(), "expected spans for fn");
        // Find the "fn" keyword span.
        let kw_idx = HIGHLIGHT_NAMES.iter().position(|n| *n == "keyword").unwrap();
        let has_kw = spans
            .iter()
            .any(|s| s.scope == kw_idx && &src[s.range.clone()] == b"fn");
        assert!(has_kw, "expected keyword span for fn; got {:?}", spans);
    }

    #[test]
    fn highlight_rust_string() {
        let mut s = SyntaxState::new(Language::Rust).unwrap();
        let src = b"fn f() { let x = \"hi\"; }";
        let spans = s.highlight(src).unwrap();
        let str_idx = HIGHLIGHT_NAMES.iter().position(|n| *n == "string").unwrap();
        let has_str = spans.iter().any(|s| s.scope == str_idx);
        assert!(has_str, "expected string span; got {:?}", spans);
    }
}
