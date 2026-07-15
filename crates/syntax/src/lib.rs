//! Tree-sitter highlighting for vix.
//!
//! Minimal surface: detect language from a path, parse the source, and emit
//! a flat list of byte-ranged highlight spans classified by a small set of
//! named scopes. The TUI crate owns the mapping from scope → color.

use std::ops::Range;
use std::path::Path;

use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator, Tree};
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

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
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
    Hcl,
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
            // HCL family: Terraform (.tf, .tfvars), OpenTofu (.tofu, .tofuvars),
            // Packer (.pkr.hcl / .pkrvars.hcl — only the trailing .hcl ext is
            // visible to `path.extension`), and bare HCL (.hcl, .nomad).
            "hcl" | "tf" | "tfvars" | "tofu" | "tofuvars" | "nomad" => Some(Language::Hcl),
            _ => None,
        }
    }

    /// Single-line comment prefix for this language, or `None` for languages
    /// where line comments aren't a fit (HTML/Markdown use block syntax;
    /// JSON has no comments).
    pub fn line_comment(&self) -> Option<&'static str> {
        match self {
            Language::Rust
            | Language::JavaScript
            | Language::TypeScript
            | Language::Tsx
            | Language::Go
            | Language::Css
            | Language::Hcl => Some("//"),
            Language::Python | Language::Bash | Language::Toml => Some("#"),
            Language::Markdown | Language::Html | Language::Json => None,
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

/// The tree-sitter grammar for a language. Shared by the full highlighter,
/// the windowed editor path, and the symbol picker.
fn grammar_for(lang: Language) -> tree_sitter::Language {
    match lang {
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
        Language::Hcl => tree_sitter_hcl::LANGUAGE.into(),
    }
}

/// The raw highlights query source for a language — the same string the
/// full-path `HighlightConfiguration` is built from.
fn highlights_src(lang: Language) -> &'static str {
    match lang {
        Language::Rust => tree_sitter_rust::HIGHLIGHTS_QUERY,
        Language::Python => tree_sitter_python::HIGHLIGHTS_QUERY,
        Language::JavaScript => tree_sitter_javascript::HIGHLIGHT_QUERY,
        Language::TypeScript | Language::Tsx => tree_sitter_typescript::HIGHLIGHTS_QUERY,
        Language::Go => tree_sitter_go::HIGHLIGHTS_QUERY,
        Language::Markdown => tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
        Language::Json => tree_sitter_json::HIGHLIGHTS_QUERY,
        Language::Toml => tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        Language::Html => tree_sitter_html::HIGHLIGHTS_QUERY,
        Language::Css => tree_sitter_css::HIGHLIGHTS_QUERY,
        Language::Bash => tree_sitter_bash::HIGHLIGHT_QUERY,
        Language::Hcl => HCL_HIGHLIGHTS,
    }
}

/// Whether the windowed editor path is enabled for a language. The windowed
/// path skips tree-sitter-highlight's locals machinery (definition/reference
/// tracking); for languages whose bundled queries have no locals that's
/// exactly equivalent (verified per-byte against the full path over
/// `fixtures/` — see the `windowed_*` tests). JS/TS/TSX ship LOCALS_QUERYs,
/// so a local reference could highlight differently — they stay on the full
/// path until locals are replicated.
fn windowed_supported(lang: Language) -> bool {
    !matches!(
        lang,
        Language::JavaScript | Language::TypeScript | Language::Tsx
    )
}

/// Raw-query highlighter for the editor's viewport: a retained parser and
/// tree plus the compiled highlights query. `QueryCursor::set_byte_range`
/// limits query execution to the visible window, so highlight cost stops
/// scaling with file size (the parse is still whole-file — incremental
/// `InputEdit` reparse is a later step).
///
/// Injections are irrelevant here: the full path passes a `|_| None`
/// injection callback, so injected layers were never highlighted anyway.
struct WindowedHighlighter {
    parser: Parser,
    tree: Option<Tree>,
    query: Query,
    /// Query capture index → `HIGHLIGHT_NAMES` index, using the same
    /// best-match rule as `HighlightConfiguration::configure`.
    capture_scopes: Vec<Option<usize>>,
}

impl WindowedHighlighter {
    fn new(lang: Language) -> Option<Self> {
        if !windowed_supported(lang) {
            return None;
        }
        let grammar = grammar_for(lang);
        let mut parser = Parser::new();
        parser.set_language(&grammar).ok()?;
        let query = Query::new(&grammar, highlights_src(lang)).ok()?;
        let capture_scopes = query
            .capture_names()
            .iter()
            .map(|name| best_scope_for(name))
            .collect();
        Some(Self {
            parser,
            tree: None,
            query,
            capture_scopes,
        })
    }
}

/// Map a query capture name to the best `HIGHLIGHT_NAMES` entry — the exact
/// rule `HighlightConfiguration::configure` uses: every dot-part of the
/// recognized name must appear among the capture name's parts; the
/// recognized name with the most parts wins, first-listed on ties.
fn best_scope_for(capture_name: &str) -> Option<usize> {
    let capture_parts: Vec<&str> = capture_name.split('.').collect();
    let mut best: Option<usize> = None;
    let mut best_len = 0;
    for (i, recognized) in HIGHLIGHT_NAMES.iter().enumerate() {
        let mut len = 0;
        let mut matches = true;
        for part in recognized.split('.') {
            len += 1;
            if !capture_parts.contains(&part) {
                matches = false;
                break;
            }
        }
        if matches && len > best_len {
            best = Some(i);
            best_len = len;
        }
    }
    best
}

/// Stateful highlighter. One per buffer. The full path (`highlight`)
/// re-parses from source on each call; the windowed path
/// (`highlight_range`) retains its parse tree across calls when the source
/// hasn't changed, and only executes the query over the requested window.
pub struct SyntaxState {
    lang: Language,
    cfg: HighlightConfiguration,
    hl: Highlighter,
    windowed: Option<WindowedHighlighter>,
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
            Language::Hcl => HighlightConfiguration::new(
                tree_sitter_hcl::LANGUAGE.into(),
                "hcl",
                HCL_HIGHLIGHTS,
                "",
                "",
            )?,
        };
        cfg.configure(HIGHLIGHT_NAMES);
        Ok(Self {
            lang,
            cfg,
            hl: Highlighter::new(),
            windowed: WindowedHighlighter::new(lang),
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
            Language::Hcl => tree_sitter_hcl::LANGUAGE.into(),
        };
        let mut parser = Parser::new();
        parser.set_language(&ts_lang)?;
        let Some(tree) = parser.parse(source, None) else {
            return Ok(Vec::new());
        };
        let query = Query::new(&ts_lang, query_str)?;
        let capture_names = query.capture_names();
        let name_idx = capture_names.iter().position(|n| *n == "name");
        let Some(name_idx) = name_idx else {
            return Ok(Vec::new());
        };
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

    /// Viewport-limited highlight for the editor's hot path. Returns spans
    /// whose per-byte scopes inside `range` are identical to what
    /// [`Self::highlight`] would produce for the whole file (spans may
    /// extend past the window; callers window per-line anyway). Returns
    /// `None` when this language must use the full path (locals-dependent
    /// grammars) or parsing fails — the caller falls back to `highlight`.
    ///
    /// `source_changed` = false lets the retained tree be reused, skipping
    /// the parse entirely (pure scrolling costs one windowed query run).
    pub fn highlight_range(
        &mut self,
        source: &[u8],
        range: Range<usize>,
        source_changed: bool,
    ) -> Option<Vec<HlSpan>> {
        let w = self.windowed.as_mut()?;
        if source_changed || w.tree.is_none() {
            // Full fresh parse: without byte-level edit deltas, reusing the
            // old tree via `parse(_, Some(old))` would be unsound. The win
            // here is the windowed *query*; incremental parse is a later
            // step once edit plumbing exists.
            w.tree = w.parser.parse(source, None);
        }
        let tree = w.tree.as_ref()?;

        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range);
        let mut captures = cursor.captures(&w.query, tree.root_node(), source);

        // Resolve captures node by node, mirroring tree-sitter-highlight:
        // consecutive captures on the same node override each other (the
        // last pattern wins, even if it maps to no recognized scope).
        let mut nodes: Vec<(usize, usize, Option<usize>)> = Vec::new();
        let mut last_node_id: Option<usize> = None;
        while let Some((m, idx)) = captures.next() {
            let cap = m.captures[*idx];
            let scope = w.capture_scopes[cap.index as usize];
            if last_node_id == Some(cap.node.id()) {
                if let Some(last) = nodes.last_mut() {
                    last.2 = scope;
                }
            } else {
                nodes.push((cap.node.start_byte(), cap.node.end_byte(), scope));
                last_node_id = Some(cap.node.id());
            }
        }

        // Flatten nested scoped nodes into non-overlapping innermost-wins
        // spans — the same output shape `highlight`'s event loop produces.
        // Nodes whose final capture maps to no recognized scope emit
        // nothing, but still (correctly) suppressed any earlier capture on
        // the same node above. `pos` tracks the emit cursor; a segment is
        // attributed to the top of the open-highlight stack, exactly like
        // the event stream's Source-under-innermost-Start behavior.
        let mut spans: Vec<HlSpan> = Vec::new();
        let mut stack: Vec<(usize, usize)> = Vec::new(); // (end, scope)
        let mut pos = 0usize;
        for (start, end, scope) in nodes {
            let Some(scope) = scope else { continue };
            // Close highlights that end at or before this node's start,
            // emitting each one's trailing segment.
            while let Some(&(top_end, top_scope)) = stack.last() {
                if top_end > start {
                    break;
                }
                if pos < top_end {
                    spans.push(HlSpan {
                        range: pos..top_end,
                        scope: top_scope,
                    });
                    pos = top_end;
                }
                stack.pop();
            }
            // Segment between the emit cursor and this node's start belongs
            // to the enclosing open highlight (if any).
            if pos < start {
                if let Some(&(_, outer_scope)) = stack.last() {
                    spans.push(HlSpan {
                        range: pos..start,
                        scope: outer_scope,
                    });
                }
                pos = start;
            }
            pos = pos.max(start);
            stack.push((end, scope));
        }
        // Drain: innermost first, emitting each highlight's remaining tail.
        while let Some((top_end, top_scope)) = stack.pop() {
            if pos < top_end {
                spans.push(HlSpan {
                    range: pos..top_end,
                    scope: top_scope,
                });
                pos = top_end;
            }
        }
        Some(spans)
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
        Language::Rust => &[
            "fn", "struct", "enum", "trait", "const", "static", "macro", "mod",
        ],
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

/// Hand-rolled highlights query for HCL (Terraform, OpenTofu, Packer, Nomad,
/// bare HCL). The upstream `tree-sitter-hcl` crate ships only the parser, no
/// queries, so we maintain our own. Captures are restricted to scopes listed
/// in [`HIGHLIGHT_NAMES`].
const HCL_HIGHLIGHTS: &str = r#"
(comment) @comment

(numeric_lit) @constant
(bool_lit) @constant.builtin
(null_lit) @constant.builtin

(string_lit) @string
(template_literal) @string
(heredoc_template) @string
(heredoc_identifier) @string.special
(quoted_template_start) @string
(quoted_template_end) @string

(template_interpolation_start) @punctuation.special
(template_interpolation_end) @punctuation.special
(template_for_start) @punctuation.special
(template_for_end) @punctuation.special
(template_if_intro) @punctuation.special
(template_if_end) @punctuation.special
(template_else_intro) @punctuation.special

(block (identifier) @keyword)
(attribute (identifier) @property)
(function_call (identifier) @function)
(get_attr (identifier) @property)
(variable_expr (identifier) @variable)

["if" "else" "for" "in" "endfor" "endif"] @keyword.control

[
  "==" "!=" "<" ">" "<=" ">="
  "&&" "||" "!"
  "+" "-" "*" "/" "%"
  "?" ":" "=" "=>"
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "."] @punctuation.delimiter
"#;

const GO_SYMBOLS: &str = r#"
(function_declaration name: (identifier) @name)
(method_declaration name: (field_identifier) @name)
(type_declaration (type_spec name: (type_identifier) @name))
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-byte scope map over `window`, resolved from a flat span list.
    /// Both highlight paths produce non-overlapping spans, so this is the
    /// visual ground truth a renderer would paint.
    fn byte_scopes(spans: &[HlSpan], window: &Range<usize>) -> Vec<Option<usize>> {
        let mut v = vec![None; window.end - window.start];
        for s in spans {
            let a = s.range.start.max(window.start);
            let b = s.range.end.min(window.end);
            for slot in v
                .iter_mut()
                .take(b.saturating_sub(window.start))
                .skip(a.saturating_sub(window.start))
            {
                *slot = Some(s.scope);
            }
        }
        v
    }

    fn fixture_files() -> Vec<std::path::PathBuf> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        let mut out: Vec<_> = std::fs::read_dir(dir)
            .expect("fixtures dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| Language::from_path(p).is_some())
            .collect();
        out.sort();
        assert!(!out.is_empty(), "no language fixtures found");
        out
    }

    /// The windowed editor path must paint every byte in the window with
    /// exactly the scope the full tree-sitter-highlight path would.
    #[test]
    fn windowed_highlight_matches_full_path_per_byte() {
        for path in fixture_files() {
            let lang = Language::from_path(&path).unwrap();
            let src = std::fs::read(&path).unwrap();
            let mut state = SyntaxState::new(lang).unwrap();
            let full = state.highlight(&src).unwrap();
            if !windowed_supported(lang) {
                assert!(
                    state.highlight_range(&src, 0..src.len(), true).is_none(),
                    "{lang:?}: unsupported language must fall back"
                );
                continue;
            }
            let len = src.len();
            let windows = [
                0..len,
                len / 3..(2 * len) / 3,
                0..(200.min(len)),
                len.saturating_sub(100)..len,
            ];
            for window in windows {
                let spans = state
                    .highlight_range(&src, window.clone(), true)
                    .unwrap_or_else(|| panic!("{lang:?}: windowed path unavailable"));
                assert_eq!(
                    byte_scopes(&spans, &window),
                    byte_scopes(&full, &window),
                    "{lang:?} {}: window {window:?} diverged from full path",
                    path.display(),
                );
            }
        }
    }

    /// Scrolling reuses the retained tree: a second query over a different
    /// window with `source_changed = false` must not need a reparse and
    /// still match the full path.
    #[test]
    fn windowed_highlight_reuses_tree_across_windows() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sample.rs");
        let src = std::fs::read(&path).unwrap();
        let mut state = SyntaxState::new(Language::Rust).unwrap();
        let full = state.highlight(&src).unwrap();
        let w1 = 0..src.len() / 2;
        let w2 = src.len() / 2..src.len();
        let s1 = state.highlight_range(&src, w1.clone(), true).unwrap();
        let s2 = state.highlight_range(&src, w2.clone(), false).unwrap();
        assert_eq!(byte_scopes(&s1, &w1), byte_scopes(&full, &w1));
        assert_eq!(byte_scopes(&s2, &w2), byte_scopes(&full, &w2));
    }

    #[test]
    fn best_scope_matches_configure_semantics() {
        // "function.method" has both parts of recognized "function.method".
        let i = best_scope_for("function.method").unwrap();
        assert_eq!(HIGHLIGHT_NAMES[i], "function.method");
        // Parts are set-contained, not positional: "punctuation.definition"
        // still matches "punctuation".
        let i = best_scope_for("punctuation.definition").unwrap();
        assert_eq!(HIGHLIGHT_NAMES[i], "punctuation");
        // Unrecognized names map to nothing.
        assert!(best_scope_for("spell").is_none());
    }

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
            ("main.tf", Language::Hcl),
            ("vars.tfvars", Language::Hcl),
            ("config.hcl", Language::Hcl),
            ("ec2.pkr.hcl", Language::Hcl),
            ("opentofu.tofu", Language::Hcl),
            ("job.nomad", Language::Hcl),
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
            Language::Hcl,
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
        let kw_idx = HIGHLIGHT_NAMES
            .iter()
            .position(|n| *n == "keyword")
            .unwrap();
        let has_kw = spans
            .iter()
            .any(|s| s.scope == kw_idx && &src[s.range.clone()] == b"fn");
        assert!(has_kw, "expected keyword span for fn; got {:?}", spans);
    }

    #[test]
    fn highlight_hcl_block_and_string() {
        let mut s = SyntaxState::new(Language::Hcl).unwrap();
        let src = b"resource \"aws_instance\" \"web\" {\n  ami = \"ami-123\"\n}\n";
        let spans = s.highlight(src).unwrap();
        let kw = HIGHLIGHT_NAMES
            .iter()
            .position(|n| *n == "keyword")
            .unwrap();
        let str_idx = HIGHLIGHT_NAMES.iter().position(|n| *n == "string").unwrap();
        let prop = HIGHLIGHT_NAMES
            .iter()
            .position(|n| *n == "property")
            .unwrap();
        assert!(
            spans
                .iter()
                .any(|s| s.scope == kw && &src[s.range.clone()] == b"resource"),
            "expected keyword span for `resource`"
        );
        assert!(
            spans.iter().any(|s| s.scope == str_idx),
            "expected string span"
        );
        assert!(
            spans
                .iter()
                .any(|s| s.scope == prop && &src[s.range.clone()] == b"ami"),
            "expected property span for `ami`"
        );
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
