//! Pure markdown layout engine for the rendered view: parse + style + wrap
//! into display lines. No Editor dependency; unit-testable standalone.
//!
//! Pipeline: [`pulldown_cmark`] events (with byte ranges) are first parsed
//! into a small internal block/inline tree ([`Node`] / [`Inline`]), then that
//! tree is rendered into styled, word-wrapped [`Line`]s by [`Renderer`]. The
//! two phases are kept separate so nested containers (lists inside
//! blockquotes, etc.) can be built with ordinary recursion instead of
//! threading parser state through the renderer.

use std::ops::Range;

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, LinkType, Options, Parser as CmarkParser, Tag, TagEnd,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Theme;
use vix_syntax::HlSpan;

/// Floor on usable content width so degenerate (tiny) terminal widths can't
/// drive wrap math to zero and loop forever.
const MIN_WIDTH: usize = 4;

/// Whether a buffer displays as raw text or rendered markdown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewMode {
    Raw,
    Rendered,
}

/// A laid-out rendered-markdown document at a specific width.
// TODO(markdown-view): consumed by the editor integration.
#[allow(dead_code)]
pub(crate) struct MdLayout {
    /// Styled display lines, wrapped to `width`.
    pub lines: Vec<Line<'static>>,
    /// display line idx -> source (buffer) line idx; same len as `lines`,
    /// monotone non-decreasing. Used to seed the cursor when toggling to raw.
    pub source_lines: Vec<usize>,
    /// Content width the layout was computed for (relayout key).
    pub width: u16,
    /// Buffer version the layout was computed from (relayout key).
    pub version: u64,
}

// TODO(markdown-view): consumed by the editor integration.
#[allow(dead_code)]
pub(crate) struct MdContext<'a> {
    pub theme: &'a Theme,
    /// (fence_token, code_body) -> highlight spans for fenced code blocks.
    /// The editor supplies a closure over its per-language syntax cache.
    pub highlight_code: &'a mut dyn FnMut(&str, &str) -> Vec<HlSpan>,
}

// TODO(markdown-view): consumed by the editor integration.
#[allow(dead_code)]
pub(crate) fn layout_markdown(
    src: &str,
    width: u16,
    version: u64,
    ctx: &mut MdContext,
) -> MdLayout {
    if src.trim().is_empty() {
        return MdLayout {
            lines: Vec::new(),
            source_lines: Vec::new(),
            width,
            version,
        };
    }

    let mut line_starts = vec![0usize];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let total_lines = line_starts.len();

    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let events: Vec<(Event, Range<usize>)> = CmarkParser::new_ext(src, options)
        .into_offset_iter()
        .collect();

    let mut parser = Parser {
        events,
        pos: 0,
        line_starts,
        total_lines,
    };
    let top_nodes = parse_blocks(&mut parser, false);

    let eff_width = (width as usize).max(MIN_WIDTH);
    let mut renderer = Renderer {
        theme: ctx.theme,
        highlight_code: ctx.highlight_code,
        width: eff_width,
        text_style: Style::default(),
        list_depth: 0,
        indent: Vec::new(),
        out: Vec::new(),
        out_src: Vec::new(),
    };
    render_blocks(&top_nodes, &mut renderer, true);

    MdLayout {
        lines: renderer.out,
        source_lines: renderer.out_src,
        width,
        version,
    }
}

// ---------------------------------------------------------------------
// Event cursor + byte->line mapping
// ---------------------------------------------------------------------

struct Parser<'a> {
    events: Vec<(Event<'a>, Range<usize>)>,
    pos: usize,
    line_starts: Vec<usize>,
    total_lines: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<(Event<'a>, Range<usize>)> {
        self.events.get(self.pos).cloned()
    }

    fn next(&mut self) -> Option<(Event<'a>, Range<usize>)> {
        let e = self.peek();
        if e.is_some() {
            self.pos += 1;
        }
        e
    }

    fn byte_to_line(&self, byte: usize) -> usize {
        let idx = self.line_starts.partition_point(|&s| s <= byte);
        idx.saturating_sub(1)
            .min(self.total_lines.saturating_sub(1))
    }
}

// ---------------------------------------------------------------------
// Parsed document tree
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct InlineMods {
    bold: bool,
    italic: bool,
    strike: bool,
}

/// Flattened inline content. Text formatted with Strong/Emphasis/
/// Strikethrough keeps its modifiers; everything else (links, code,
/// footnote refs, raw html) gets a fixed style at render time regardless of
/// surrounding emphasis, matching how headings override the paragraph
/// default.
enum Inline {
    Text(String, InlineMods),
    Code(String),
    /// Also covers images (`alt` text lives in `text`, doc comment on field
    /// name kept generic so `Link`/`Image` can share render logic).
    Link {
        text: String,
        url: String,
        suffix: bool,
    },
    Image {
        text: String,
        url: String,
        suffix: bool,
    },
    FootnoteRef(String),
    Html(String),
    SoftBreak,
    HardBreak,
}

struct ListItem {
    checked: Option<bool>,
    blocks: Vec<Node>,
    src_line: usize,
}

enum Node {
    Paragraph {
        inlines: Vec<Inline>,
        src_line: usize,
    },
    Heading {
        level: u8,
        inlines: Vec<Inline>,
        src_line: usize,
    },
    BlockQuote {
        blocks: Vec<Node>,
        src_line: usize,
    },
    CodeBlock {
        token: String,
        lines: Vec<String>,
        src_line: usize,
    },
    List {
        ordered_start: Option<u64>,
        items: Vec<ListItem>,
    },
    Table {
        alignments: Vec<Alignment>,
        header: Vec<Vec<Inline>>,
        rows: Vec<(Vec<Vec<Inline>>, usize)>,
        src_line: usize,
    },
    Rule {
        src_line: usize,
    },
    Frontmatter {
        lines: Vec<String>,
        src_line: usize,
    },
    FootnoteDefinition {
        label: String,
        blocks: Vec<Node>,
        src_line: usize,
    },
    HtmlBlock {
        lines: Vec<String>,
        src_line: usize,
    },
}

fn is_block_tag(tag: &Tag) -> bool {
    !matches!(
        tag,
        Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. }
    )
}

/// Parses a sequence of block-level nodes. At the top level (`in_container
/// == false`) this runs until the event stream is exhausted; inside a
/// container it stops at (and consumes) the next `End` event, which by
/// construction of pulldown-cmark's balanced event stream is always the
/// matching close of whatever `Start` the caller consumed.
///
/// Containers that allow "tight" content (list items, footnote defs) can
/// have raw inline events directly instead of a wrapping `Paragraph`; any
/// such run is collected into an implicit `Node::Paragraph`.
fn parse_blocks(p: &mut Parser, in_container: bool) -> Vec<Node> {
    let mut nodes = Vec::new();
    loop {
        match p.peek() {
            None => break,
            Some((Event::End(_), _)) => {
                if in_container {
                    p.next();
                }
                break;
            }
            Some((Event::Start(ref tag), _)) if is_block_tag(tag) => {
                if let Some(node) = parse_block_start(p) {
                    nodes.push(node);
                }
            }
            Some((Event::Rule, range)) => {
                let src_line = p.byte_to_line(range.start);
                p.next();
                nodes.push(Node::Rule { src_line });
            }
            Some((_, range)) => {
                // Tight inline content (list items / footnote defs can hold
                // raw inline events with no wrapping `Paragraph`).
                // `parse_inline_until_end` always consumes at least this
                // event, so no extra advance is needed here.
                let src_line = p.byte_to_line(range.start);
                let inlines = parse_inline_until_end(p);
                if !inlines.is_empty() {
                    nodes.push(Node::Paragraph { inlines, src_line });
                }
            }
        }
    }
    nodes
}

fn parse_block_start(p: &mut Parser) -> Option<Node> {
    let (event, range) = p.next()?;
    let Event::Start(tag) = event else {
        return None;
    };
    let src_line = p.byte_to_line(range.start);
    match tag {
        Tag::Paragraph => {
            let inlines = parse_inline_until_end(p);
            p.next(); // consume End(Paragraph)
            Some(Node::Paragraph { inlines, src_line })
        }
        Tag::Heading { level, .. } => {
            let inlines = parse_inline_until_end(p);
            p.next(); // consume End(Heading)
            Some(Node::Heading {
                level: level as u8,
                inlines,
                src_line,
            })
        }
        Tag::BlockQuote(_) => Some(Node::BlockQuote {
            blocks: parse_blocks(p, true),
            src_line,
        }),
        Tag::CodeBlock(kind) => Some(parse_code_block(p, kind, range)),
        Tag::HtmlBlock => Some(parse_html_block(p, src_line)),
        Tag::List(ordered_start) => Some(Node::List {
            ordered_start,
            items: parse_list_items(p),
        }),
        Tag::Item => Some(Node::List {
            ordered_start: None,
            items: vec![ListItem {
                checked: None,
                blocks: parse_blocks(p, true),
                src_line,
            }],
        }),
        Tag::FootnoteDefinition(label) => Some(Node::FootnoteDefinition {
            label: label.to_string(),
            blocks: parse_blocks(p, true),
            src_line,
        }),
        Tag::Table(aligns) => Some(parse_table(p, aligns, src_line)),
        Tag::MetadataBlock(_) => Some(parse_metadata_block(p, src_line)),
        Tag::TableHead | Tag::TableRow | Tag::TableCell => {
            let _ = parse_blocks(p, true);
            None
        }
        _ => {
            let _ = parse_blocks(p, true);
            None
        }
    }
}

struct PendingLink {
    is_image: bool,
    link_type: LinkType,
    url: String,
    text: String,
}

fn mods_for(depth: [u32; 3]) -> InlineMods {
    InlineMods {
        bold: depth[0] > 0,
        italic: depth[1] > 0,
        strike: depth[2] > 0,
    }
}

fn push_text(out: &mut Vec<Inline>, link_stack: &mut [PendingLink], s: String, mods: InlineMods) {
    if let Some(top) = link_stack.last_mut() {
        top.text.push_str(&s);
    } else {
        out.push(Inline::Text(s, mods));
    }
}

fn push_inline(out: &mut Vec<Inline>, link_stack: &mut [PendingLink], inline: Inline) {
    if let Some(top) = link_stack.last_mut() {
        match inline {
            Inline::Text(s, _) | Inline::Code(s) | Inline::Html(s) => top.text.push_str(&s),
            Inline::FootnoteRef(label) => {
                top.text.push_str("[^");
                top.text.push_str(&label);
                top.text.push(']');
            }
            Inline::SoftBreak | Inline::HardBreak => top.text.push(' '),
            Inline::Link { text, .. } | Inline::Image { text, .. } => top.text.push_str(&text),
        }
    } else {
        out.push(inline);
    }
}

/// Consumes inline-level events (text, code spans, emphasis, links...) until
/// either the enclosing block's `End` or a nested block-level `Start` — both
/// left unconsumed for the caller to handle (the former: `Paragraph`/
/// `Heading`/`TableCell` consume it themselves; the latter: a tight list
/// item's content followed by a nested sub-list).
fn parse_inline_until_end(p: &mut Parser) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::new();
    // [bold, italic, strike] nesting depth.
    let mut depth = [0u32; 3];
    let mut link_stack: Vec<PendingLink> = Vec::new();

    while let Some((event, _range)) = p.peek() {
        match &event {
            Event::Start(tag) if is_block_tag(tag) => break,
            Event::Start(Tag::Emphasis) => {
                p.next();
                depth[1] += 1;
            }
            Event::Start(Tag::Strong) => {
                p.next();
                depth[0] += 1;
            }
            Event::Start(Tag::Strikethrough) => {
                p.next();
                depth[2] += 1;
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => {
                let lt = *link_type;
                let url = dest_url.to_string();
                p.next();
                link_stack.push(PendingLink {
                    is_image: false,
                    link_type: lt,
                    url,
                    text: String::new(),
                });
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                ..
            }) => {
                let lt = *link_type;
                let url = dest_url.to_string();
                p.next();
                link_stack.push(PendingLink {
                    is_image: true,
                    link_type: lt,
                    url,
                    text: String::new(),
                });
            }
            Event::Start(_) => {
                p.next();
            }
            Event::End(TagEnd::Emphasis) => {
                p.next();
                depth[1] = depth[1].saturating_sub(1);
            }
            Event::End(TagEnd::Strong) => {
                p.next();
                depth[0] = depth[0].saturating_sub(1);
            }
            Event::End(TagEnd::Strikethrough) => {
                p.next();
                depth[2] = depth[2].saturating_sub(1);
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {
                p.next();
                if let Some(pl) = link_stack.pop() {
                    let suffix = !matches!(pl.link_type, LinkType::Autolink | LinkType::Email)
                        && pl.url != pl.text;
                    let inline = if pl.is_image {
                        Inline::Image {
                            text: pl.text,
                            url: pl.url,
                            suffix,
                        }
                    } else {
                        Inline::Link {
                            text: pl.text,
                            url: pl.url,
                            suffix,
                        }
                    };
                    push_inline(&mut out, &mut link_stack, inline);
                }
            }
            // The enclosing block's own End (Paragraph/Heading/TableCell/
            // tight Item content...): left unconsumed. Callers either
            // consume it themselves (Paragraph/Heading/TableCell, which own
            // exactly one such End) or, for the tight-list-item fallback in
            // `parse_blocks`, leave it for that function's own End-handling
            // arm so container termination stays in one place.
            Event::End(_) => break,
            Event::Text(s) => {
                let s = s.to_string();
                let mods = mods_for(depth);
                p.next();
                push_text(&mut out, &mut link_stack, s, mods);
            }
            Event::Code(s) => {
                let s = s.to_string();
                p.next();
                push_inline(&mut out, &mut link_stack, Inline::Code(s));
            }
            Event::InlineHtml(s) => {
                let s = s.to_string();
                p.next();
                push_inline(&mut out, &mut link_stack, Inline::Html(s));
            }
            Event::FootnoteReference(s) => {
                let s = s.to_string();
                p.next();
                push_inline(&mut out, &mut link_stack, Inline::FootnoteRef(s));
            }
            Event::SoftBreak => {
                p.next();
                push_inline(&mut out, &mut link_stack, Inline::SoftBreak);
            }
            Event::HardBreak => {
                p.next();
                push_inline(&mut out, &mut link_stack, Inline::HardBreak);
            }
            _ => {
                p.next();
            }
        }
    }
    out
}

fn parse_list_items(p: &mut Parser) -> Vec<ListItem> {
    let mut items = Vec::new();
    loop {
        match p.peek() {
            Some((Event::Start(Tag::Item), range)) => {
                let src_line = p.byte_to_line(range.start);
                p.next();
                let checked = if let Some((Event::TaskListMarker(b), _)) = p.peek() {
                    p.next();
                    Some(b)
                } else {
                    None
                };
                let blocks = parse_blocks(p, true);
                items.push(ListItem {
                    checked,
                    blocks,
                    src_line,
                });
            }
            Some((Event::End(_), _)) => {
                p.next();
                break;
            }
            None => break,
            _ => {
                p.next();
            }
        }
    }
    items
}

fn parse_table(p: &mut Parser, aligns: Vec<Alignment>, src_line: usize) -> Node {
    let mut header: Vec<Vec<Inline>> = Vec::new();
    let mut rows: Vec<(Vec<Vec<Inline>>, usize)> = Vec::new();
    loop {
        match p.peek() {
            Some((Event::Start(Tag::TableHead), _)) => {
                p.next();
                header = parse_table_row_cells(p);
            }
            Some((Event::Start(Tag::TableRow), range)) => {
                let row_src = p.byte_to_line(range.start);
                p.next();
                rows.push((parse_table_row_cells(p), row_src));
            }
            Some((Event::End(_), _)) => {
                p.next();
                break;
            }
            None => break,
            _ => {
                p.next();
            }
        }
    }
    Node::Table {
        alignments: aligns,
        header,
        rows,
        src_line,
    }
}

fn parse_table_row_cells(p: &mut Parser) -> Vec<Vec<Inline>> {
    let mut cells = Vec::new();
    loop {
        match p.peek() {
            Some((Event::Start(Tag::TableCell), _)) => {
                p.next();
                cells.push(parse_inline_until_end(p));
                p.next(); // consume End(TableCell)
            }
            Some((Event::End(_), _)) => {
                p.next();
                break;
            }
            None => break,
            _ => {
                p.next();
            }
        }
    }
    cells
}

fn parse_code_block(p: &mut Parser, kind: CodeBlockKind, block_range: Range<usize>) -> Node {
    let token = match &kind {
        CodeBlockKind::Fenced(t) => t.to_string(),
        CodeBlockKind::Indented => String::new(),
    };
    let fenced = kind.is_fenced();
    let mut text = String::new();
    let mut first_line: Option<usize> = None;
    loop {
        match p.peek() {
            Some((Event::Text(s), range)) => {
                if first_line.is_none() {
                    first_line = Some(p.byte_to_line(range.start));
                }
                text.push_str(&s);
                p.next();
            }
            Some((Event::End(_), _)) => {
                p.next();
                break;
            }
            None => break,
            _ => {
                p.next();
            }
        }
    }
    let src_line =
        first_line.unwrap_or_else(|| p.byte_to_line(block_range.start) + usize::from(fenced));
    Node::CodeBlock {
        token,
        lines: split_body_lines(&text),
        src_line,
    }
}

fn parse_html_block(p: &mut Parser, block_src_line: usize) -> Node {
    let mut text = String::new();
    let mut first_line: Option<usize> = None;
    loop {
        match p.peek() {
            Some((Event::Html(s), range)) | Some((Event::Text(s), range)) => {
                if first_line.is_none() {
                    first_line = Some(p.byte_to_line(range.start));
                }
                text.push_str(&s);
                p.next();
            }
            Some((Event::End(_), _)) => {
                p.next();
                break;
            }
            None => break,
            _ => {
                p.next();
            }
        }
    }
    Node::HtmlBlock {
        lines: split_body_lines(&text),
        src_line: first_line.unwrap_or(block_src_line),
    }
}

fn parse_metadata_block(p: &mut Parser, block_src_line: usize) -> Node {
    let mut text = String::new();
    let mut first_line: Option<usize> = None;
    loop {
        match p.peek() {
            Some((Event::Text(s), range)) => {
                if first_line.is_none() {
                    first_line = Some(p.byte_to_line(range.start));
                }
                text.push_str(&s);
                p.next();
            }
            Some((Event::End(_), _)) => {
                p.next();
                break;
            }
            None => break,
            _ => {
                p.next();
            }
        }
    }
    Node::Frontmatter {
        lines: split_body_lines(&text),
        src_line: first_line.unwrap_or(block_src_line),
    }
}

fn split_body_lines(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<String> = s.split('\n').map(|x| x.to_string()).collect();
    if v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v
}

// ---------------------------------------------------------------------
// Tokenizer: inline tree -> wrappable clusters
// ---------------------------------------------------------------------

/// One wrap-unit: either an atomic run of same-line text (never split
/// across a line break, though it may itself be multi-styled — e.g.
/// `**bold**tail` glues into one cluster), a breakable space, or a forced
/// line break.
enum Tok {
    Cluster(Vec<(String, Style)>),
    Space,
    Break,
}

struct TokBuilder {
    toks: Vec<Tok>,
    cur: Vec<(String, Style)>,
}

impl TokBuilder {
    fn new() -> Self {
        Self {
            toks: Vec::new(),
            cur: Vec::new(),
        }
    }

    fn flush(&mut self) {
        if !self.cur.is_empty() {
            self.toks.push(Tok::Cluster(std::mem::take(&mut self.cur)));
        }
    }

    fn space(&mut self) {
        self.flush();
        if !self.toks.is_empty() && !matches!(self.toks.last(), Some(Tok::Space | Tok::Break)) {
            self.toks.push(Tok::Space);
        }
    }

    fn brk(&mut self) {
        self.flush();
        self.toks.push(Tok::Break);
    }

    fn word(&mut self, part: &str, style: Style) {
        self.cur.push((part.to_string(), style));
    }

    fn finish(mut self) -> Vec<Tok> {
        self.flush();
        self.toks
    }
}

/// Splits `s` on whitespace into breakable words, gluing the first/last
/// word to whatever precedes/follows if `s` doesn't itself start/end with
/// whitespace (so `**bold**tail` renders as one unbreakable "boldtail").
fn feed_text(tb: &mut TokBuilder, s: &str, style: Style) {
    if s.is_empty() {
        return;
    }
    let starts_ws = s.chars().next().is_some_and(char::is_whitespace);
    let ends_ws = s.chars().next_back().is_some_and(char::is_whitespace);
    if starts_ws {
        tb.space();
    }
    let mut first = true;
    for part in s.split_whitespace() {
        if !first {
            tb.space();
        }
        tb.word(part, style);
        first = false;
    }
    if ends_ws {
        tb.space();
    }
}

/// Feeds `s` as a single unbreakable word (inline code, urls, footnote
/// refs...), still respecting surrounding glue via neighboring events.
fn feed_atomic(tb: &mut TokBuilder, s: &str, style: Style) {
    if !s.is_empty() {
        tb.word(s, style);
    }
}

fn mods_style(mods: InlineMods) -> Style {
    let mut s = Style::default();
    if mods.bold {
        s = s.add_modifier(Modifier::BOLD);
    }
    if mods.italic {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if mods.strike {
        s = s.add_modifier(Modifier::CROSSED_OUT);
    }
    s
}

fn tokenize(inlines: &[Inline], base_style: Style, theme: &Theme) -> Vec<Tok> {
    let mut tb = TokBuilder::new();
    for inl in inlines {
        match inl {
            Inline::Text(s, mods) => feed_text(&mut tb, s, base_style.patch(mods_style(*mods))),
            Inline::Code(s) => feed_atomic(&mut tb, s, theme.md.code_inline),
            Inline::Link { text, url, suffix } | Inline::Image { text, url, suffix } => {
                feed_text(&mut tb, text, theme.md.link);
                if *suffix {
                    tb.space();
                    feed_atomic(&mut tb, &format!("({url})"), theme.md.link_url);
                }
            }
            Inline::FootnoteRef(label) => {
                feed_atomic(&mut tb, &format!("[^{label}]"), theme.md.link_url)
            }
            Inline::Html(s) => feed_text(&mut tb, s, theme.md.frontmatter),
            Inline::SoftBreak => tb.space(),
            Inline::HardBreak => tb.brk(),
        }
    }
    tb.finish()
}

/// Flattens tokens onto one line (used for table cells, which aren't
/// wrapped): returns the styled spans and their total display width.
fn flatten_cell(inlines: &[Inline], base_style: Style, theme: &Theme) -> CellSpans {
    let tokens = tokenize(inlines, base_style, theme);
    let mut spans = Vec::new();
    let mut w = 0usize;
    let mut pending_space = false;
    let mut any = false;
    for tok in &tokens {
        match tok {
            Tok::Space | Tok::Break => {
                if any {
                    pending_space = true;
                }
            }
            Tok::Cluster(runs) => {
                if pending_space {
                    spans.push(Span::raw(" "));
                    w += 1;
                    pending_space = false;
                }
                for (t, s) in runs {
                    w += t.width();
                    spans.push(Span::styled(t.clone(), *s));
                }
                any = true;
            }
        }
    }
    (spans, w)
}

// ---------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------

/// A piece of persistent left-hand decoration for the current nesting
/// context. `Bar` (blockquote) repeats on every line; `Marker` (list
/// bullet / ordered number / task glyph / footnote label) renders `first`
/// once then `cont` (equal display width) for every following line, for as
/// long as it stays on the indent stack.
enum IndentSeg {
    Bar,
    Marker {
        first: Vec<Span<'static>>,
        cont: Vec<Span<'static>>,
        width: usize,
        used: bool,
    },
}

struct Renderer<'r> {
    theme: &'r Theme,
    highlight_code: &'r mut dyn FnMut(&str, &str) -> Vec<HlSpan>,
    /// Effective (floored) width used for wrap math.
    width: usize,
    /// Current inline-text base style (plain, or `md.blockquote` while
    /// inside a blockquote).
    text_style: Style,
    list_depth: usize,
    indent: Vec<IndentSeg>,
    out: Vec<Line<'static>>,
    out_src: Vec<usize>,
}

impl<'r> Renderer<'r> {
    fn indent_width(&self) -> usize {
        self.indent
            .iter()
            .map(|seg| match seg {
                IndentSeg::Bar => 2,
                IndentSeg::Marker { width, .. } => *width,
            })
            .sum()
    }

    fn avail_width(&self) -> usize {
        self.width.saturating_sub(self.indent_width()).max(1)
    }

    fn current_prefix(&mut self) -> Vec<Span<'static>> {
        let bar_style = self.theme.md.blockquote_bar;
        let mut spans = Vec::new();
        for seg in self.indent.iter_mut() {
            match seg {
                IndentSeg::Bar => spans.push(Span::styled("▌ ", bar_style)),
                IndentSeg::Marker {
                    first, cont, used, ..
                } => {
                    if *used {
                        spans.extend(cont.iter().cloned());
                    } else {
                        spans.extend(first.iter().cloned());
                        *used = true;
                    }
                }
            }
        }
        spans
    }

    fn emit_line(&mut self, content: Vec<Span<'static>>, src_line: usize) {
        let mut spans = self.current_prefix();
        spans.extend(content);
        self.out.push(Line::from(spans));
        let clamped = src_line.max(self.out_src.last().copied().unwrap_or(0));
        self.out_src.push(clamped);
    }
}

fn render_blocks(nodes: &[Node], r: &mut Renderer, top_level: bool) {
    for (i, node) in nodes.iter().enumerate() {
        if top_level && i > 0 {
            let src = r.out_src.last().copied().unwrap_or(0);
            r.emit_line(Vec::new(), src);
        }
        render_node(node, r);
    }
}

fn render_node(node: &Node, r: &mut Renderer) {
    match node {
        Node::Paragraph { inlines, src_line } => {
            let tokens = tokenize(inlines, r.text_style, r.theme);
            render_wrapped(&tokens, r, *src_line);
        }
        Node::Heading {
            level,
            inlines,
            src_line,
        } => {
            let style = heading_style(*level, r.theme);
            let tokens = tokenize(inlines, style, r.theme);
            render_wrapped(&tokens, r, *src_line);
        }
        Node::BlockQuote { blocks, src_line } => {
            let prev = r.text_style;
            r.text_style = r.theme.md.blockquote;
            r.indent.push(IndentSeg::Bar);
            let before = r.out.len();
            render_blocks(blocks, r, false);
            if r.out.len() == before {
                r.emit_line(Vec::new(), *src_line);
            }
            r.indent.pop();
            r.text_style = prev;
        }
        Node::CodeBlock {
            token,
            lines,
            src_line,
        } => {
            let is_mermaid = token
                .split_whitespace()
                .next()
                .is_some_and(|w| w.eq_ignore_ascii_case("mermaid"));
            if !is_mermaid || !render_mermaid(lines, *src_line, r) {
                render_code_block(token, lines, *src_line, r);
            }
        }
        Node::List {
            ordered_start,
            items,
        } => render_list(*ordered_start, items, r),
        Node::Table {
            alignments,
            header,
            rows,
            src_line,
        } => render_table(alignments, header, rows, *src_line, r),
        Node::Rule { src_line } => render_rule(r, *src_line),
        Node::Frontmatter { lines, src_line } => render_frontmatter(lines, *src_line, r),
        Node::FootnoteDefinition {
            label,
            blocks,
            src_line,
        } => render_footnote_def(label, blocks, *src_line, r),
        Node::HtmlBlock { lines, src_line } => render_html_block(lines, *src_line, r),
    }
}

fn heading_style(level: u8, theme: &Theme) -> Style {
    match level {
        1 => theme.md.h1,
        2 => theme.md.h2,
        3 => theme.md.h3,
        _ => theme.md.h4,
    }
}

fn render_wrapped(tokens: &[Tok], r: &mut Renderer, src_line: usize) {
    let avail = r.avail_width();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut w = 0usize;
    let mut has_content = false;
    let mut pending_space = false;

    for tok in tokens {
        match tok {
            Tok::Space => {
                if has_content {
                    pending_space = true;
                }
            }
            Tok::Break => {
                r.emit_line(std::mem::take(&mut spans), src_line);
                w = 0;
                has_content = false;
                pending_space = false;
            }
            Tok::Cluster(runs) => {
                let cw: usize = runs.iter().map(|(t, _)| t.width()).sum();
                let sep = usize::from(pending_space && has_content);
                if has_content && w + sep + cw > avail {
                    r.emit_line(std::mem::take(&mut spans), src_line);
                    w = 0;
                    has_content = false;
                    pending_space = false;
                }
                if pending_space && has_content {
                    spans.push(Span::raw(" "));
                    w += 1;
                }
                for (t, s) in runs {
                    spans.push(Span::styled(t.clone(), *s));
                }
                w += cw;
                has_content = true;
                pending_space = false;
            }
        }
    }
    if has_content {
        r.emit_line(spans, src_line);
    }
}

fn render_list(ordered_start: Option<u64>, items: &[ListItem], r: &mut Renderer) {
    let depth = r.list_depth;
    for (i, item) in items.iter().enumerate() {
        let (marker_text, marker_style) =
            marker_for(item.checked, ordered_start, i, depth, r.theme);
        let w = marker_text.width();
        let first = vec![Span::styled(marker_text, marker_style)];
        let cont = vec![Span::raw(" ".repeat(w))];
        r.indent.push(IndentSeg::Marker {
            first,
            cont,
            width: w,
            used: false,
        });
        r.list_depth = depth + 1;
        let before = r.out.len();
        render_blocks(&item.blocks, r, false);
        if r.out.len() == before {
            r.emit_line(Vec::new(), item.src_line);
        }
        r.list_depth = depth;
        r.indent.pop();
    }
}

fn marker_for(
    checked: Option<bool>,
    ordered_start: Option<u64>,
    index: usize,
    depth: usize,
    theme: &Theme,
) -> (String, Style) {
    if let Some(done) = checked {
        if done {
            ("☑ ".to_string(), theme.md.task_done)
        } else {
            ("☐ ".to_string(), theme.md.task_todo)
        }
    } else if let Some(start) = ordered_start {
        (
            format!("{}. ", start.saturating_add(index as u64)),
            theme.md.bullet,
        )
    } else {
        const GLYPHS: [&str; 3] = ["• ", "◦ ", "▪ "];
        (GLYPHS[depth % GLYPHS.len()].to_string(), theme.md.bullet)
    }
}

fn render_rule(r: &mut Renderer, src_line: usize) {
    let avail = r.avail_width();
    let style = r.theme.md.rule;
    r.emit_line(vec![Span::styled("─".repeat(avail), style)], src_line);
}

fn render_frontmatter(lines: &[String], src_line: usize, r: &mut Renderer) {
    let style = r.theme.md.frontmatter;
    for (i, line) in lines.iter().enumerate() {
        r.emit_line(vec![Span::styled(format!("▌ {line}"), style)], src_line + i);
    }
}

fn render_html_block(lines: &[String], src_line: usize, r: &mut Renderer) {
    let style = r.theme.md.frontmatter;
    for (i, line) in lines.iter().enumerate() {
        r.emit_line(vec![Span::styled(line.clone(), style)], src_line + i);
    }
}

fn render_footnote_def(label: &str, blocks: &[Node], src_line: usize, r: &mut Renderer) {
    let marker = format!("[^{label}]: ");
    let w = marker.width();
    r.indent.push(IndentSeg::Marker {
        first: vec![Span::styled(marker, r.theme.md.link_url)],
        cont: vec![Span::raw(" ".repeat(w))],
        width: w,
        used: false,
    });
    let before = r.out.len();
    render_blocks(blocks, r, false);
    if r.out.len() == before {
        r.emit_line(Vec::new(), src_line);
    }
    r.indent.pop();
}

/// Bridge the gap between mermaid.js label semantics (labels are HTML) and
/// `mermaid-text`'s literal parser: decode the common HTML entities —
/// `&lt;` inside a label explodes its lexer into bogus extra nodes — and
/// unwrap `["..."]`-style label quotes, which mermaid.js strips but the
/// crate renders verbatim. `<br/>` is left alone (the crate supports it
/// natively as a label line break).
fn preprocess_mermaid(src: &str) -> String {
    // `&amp;` last, so `&amp;lt;` decodes to the literal text `&lt;`.
    let decoded = src
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    unwrap_quoted_labels(&decoded)
}

/// Rewrite `["label"]` / `("label")` / `{"label"}` to `[label]` etc., but
/// only when the label contains neither a `"` nor its own closing bracket
/// (either would change how the unquoted form parses).
fn unwrap_quoted_labels(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let closer = match c {
            '[' => Some(']'),
            '(' => Some(')'),
            '{' => Some('}'),
            _ => None,
        };
        if let Some(closer) = closer {
            if chars.get(i + 1) == Some(&'"') {
                if let Some(off) = chars[i + 2..].iter().position(|&c| c == '"') {
                    let end = i + 2 + off;
                    let inner: String = chars[i + 2..end].iter().collect();
                    if chars.get(end + 1) == Some(&closer) && !inner.contains(closer) {
                        out.push(c);
                        out.push_str(&inner);
                        out.push(closer);
                        i = end + 2;
                        continue;
                    }
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Renders a ```mermaid fence as a Unicode box-drawing diagram via
/// `mermaid-text`. Returns `false` (no lines emitted) on any failure so the
/// caller can fall back to the plain code-block gutter rendering — the
/// crate is lenient (malformed input usually still yields `Ok` with a
/// best-effort diagram) but `Err` is possible (e.g. empty body, unsupported
/// diagram header), and an all-whitespace `Ok` is treated the same as a
/// failure.
fn render_mermaid(body_lines: &[String], src_line: usize, r: &mut Renderer) -> bool {
    let avail = r.avail_width();
    let body = preprocess_mermaid(&body_lines.join("\n"));
    let Ok(rendered) = mermaid_text::render_with_width(&body, Some(avail)) else {
        return false;
    };
    let lines: Vec<&str> = rendered.trim_end_matches(['\n', '\r']).lines().collect();
    if lines.iter().all(|l| l.trim().is_empty()) {
        return false;
    }
    // The diagram's visual line count has no 1:1 relationship to the
    // source body (a 2-line flowchart can lay out to a dozen rows), so
    // clamp the mapped source line to the body's own span instead of
    // walking past the end of the document like `render_code_block`'s
    // `src_line + i` (safe there since code output is always <= input
    // lines).
    let max_src_i = body_lines.len().saturating_sub(1);
    for (i, line) in lines.iter().enumerate() {
        r.emit_line(
            vec![Span::raw(line.trim_end().to_string())],
            src_line + i.min(max_src_i),
        );
    }
    true
}

fn render_code_block(token: &str, body_lines: &[String], src_line: usize, r: &mut Renderer) {
    let body = body_lines.join("\n");
    let spans = (r.highlight_code)(token, &body);
    let styled = style_code_lines(&body, &spans, r.theme);
    let avail_total = r.width.saturating_sub(r.indent_width()).max(1);
    let avail_content = avail_total.saturating_sub(2).max(1);
    let gutter_style = r.theme.md.code_gutter;
    for (i, line) in styled.iter().enumerate() {
        let (kept, truncated) = truncate_styled(line, avail_content);
        let mut spans_out = vec![Span::styled("│ ", gutter_style)];
        spans_out.extend(spans_from_chars(&kept));
        if truncated {
            spans_out.push(Span::raw("…"));
        }
        r.emit_line(spans_out, src_line + i);
    }
}

fn style_code_lines(
    body: &str,
    spans: &[HlSpan],
    theme: &Theme,
) -> Vec<Vec<(char, Option<Style>)>> {
    if body.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<Vec<(char, Option<Style>)>> = Vec::new();
    let mut cur: Vec<(char, Option<Style>)> = Vec::new();
    let mut byte = 0usize;
    let mut idx = 0usize;
    for ch in body.chars() {
        let len = ch.len_utf8();
        while idx < spans.len() && spans[idx].range.end <= byte {
            idx += 1;
        }
        let style =
            if idx < spans.len() && spans[idx].range.start <= byte && byte < spans[idx].range.end {
                theme.scope_styles.get(spans[idx].scope).copied().flatten()
            } else {
                None
            };
        if ch == '\n' {
            lines.push(std::mem::take(&mut cur));
        } else {
            cur.push((ch, style));
        }
        byte += len;
    }
    lines.push(cur);
    lines
}

fn truncate_styled(
    chars: &[(char, Option<Style>)],
    max_w: usize,
) -> (Vec<(char, Option<Style>)>, bool) {
    let total: usize = chars.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
    if total <= max_w {
        return (chars.to_vec(), false);
    }
    let budget = max_w.saturating_sub(1);
    let mut out = Vec::new();
    let mut w = 0usize;
    for &(c, s) in chars {
        let cw = c.width().unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push((c, s));
        w += cw;
    }
    (out, true)
}

fn spans_from_chars(chars: &[(char, Option<Style>)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let style = chars[i].1;
        let mut j = i + 1;
        while j < chars.len() && chars[j].1 == style {
            j += 1;
        }
        let text: String = chars[i..j].iter().map(|(c, _)| *c).collect();
        spans.push(match style {
            Some(s) => Span::styled(text, s),
            None => Span::raw(text),
        });
        i = j;
    }
    spans
}

/// A rendered table cell: its styled spans plus their total display width.
type CellSpans = (Vec<Span<'static>>, usize);

/// A cell word-wrapped to a fixed column width: one entry per display
/// line, same shape as [`CellSpans`] (styled spans + that line's display
/// width).
type WrappedCell = Vec<CellSpans>;

/// Floor on a table column's width once wrapping kicks in. Below
/// `ncols * MIN_TABLE_COL + 3*(ncols-1)` even shrunk columns would be
/// unreadable, so [`render_table`] keeps the old truncate-with-`…` path
/// instead.
const MIN_TABLE_COL: usize = 5;

fn render_table(
    aligns: &[Alignment],
    header: &[Vec<Inline>],
    rows: &[(Vec<Vec<Inline>>, usize)],
    src_line: usize,
    r: &mut Renderer,
) {
    let ncols = header.len().max(aligns.len());
    let header_cells: Vec<CellSpans> = header
        .iter()
        .map(|c| flatten_cell(c, r.theme.md.table_header, r.theme))
        .collect();
    let body_rows: Vec<(Vec<CellSpans>, usize)> = rows
        .iter()
        .map(|(cells, rsrc)| {
            (
                cells
                    .iter()
                    .map(|c| flatten_cell(c, r.text_style, r.theme))
                    .collect(),
                *rsrc,
            )
        })
        .collect();

    let mut widths = vec![0usize; ncols];
    for (i, (_, w)) in header_cells.iter().enumerate() {
        if i < ncols {
            widths[i] = widths[i].max(*w);
        }
    }
    for (cells, _) in &body_rows {
        for (i, (_, w)) in cells.iter().enumerate() {
            if i < ncols {
                widths[i] = widths[i].max(*w);
            }
        }
    }

    let avail = r.avail_width();
    let border = r.theme.md.table_border;

    // Natural (widest-cell) column widths, as always. If they fit `avail`
    // as-is this is exactly today's single-line-per-row behavior; if they
    // don't fit but the pane is too narrow to give every column even
    // `MIN_TABLE_COL`, shrinking wouldn't help either, so fall back to the
    // original truncate-rightmost-columns-with-`…` behavior unchanged.
    let sep_total = 3 * ncols.saturating_sub(1);
    let naturals_fit = widths.iter().sum::<usize>() + sep_total <= avail;
    let min_fits = ncols * MIN_TABLE_COL + sep_total <= avail;
    if naturals_fit || !min_fits {
        let header_spans = render_row(&header_cells, &widths, aligns, avail, border);
        r.emit_line(header_spans, src_line);
        let rule_spans = render_rule_row(&widths, avail, border);
        r.emit_line(rule_spans, src_line);
        for (cells, rsrc) in &body_rows {
            let spans = render_row(cells, &widths, aligns, avail, border);
            r.emit_line(spans, *rsrc);
        }
        return;
    }

    // Overwide but shrinkable: fair-share the available width across
    // columns and word-wrap each cell to its allocated width instead of
    // truncating.
    let col_widths = alloc_col_widths(&widths, avail);
    render_wrapped_row(&header_cells, &col_widths, aligns, border, src_line, r);
    let rule_spans = render_rule_row(&col_widths, avail, border);
    r.emit_line(rule_spans, src_line);
    for (cells, rsrc) in &body_rows {
        render_wrapped_row(cells, &col_widths, aligns, border, *rsrc, r);
    }
}

/// Classic fair-share column allocation, used once naturals don't fit
/// `avail` (but `ncols * MIN_TABLE_COL + 3*(ncols-1) <= avail`, checked by
/// the caller). Columns whose natural width is already <= the evenly-split
/// remainder are fixed at their natural width (freeing their slack for the
/// rest); once no more columns qualify, whatever's left is split evenly
/// across the remaining ("wide") columns, handing the remainder out one
/// extra column at a time, left to right.
fn alloc_col_widths(naturals: &[usize], avail: usize) -> Vec<usize> {
    let ncols = naturals.len();
    if ncols == 0 {
        return Vec::new();
    }
    let avail_content = avail.saturating_sub(3 * (ncols - 1));
    let mut fixed = vec![false; ncols];
    let mut widths = vec![0usize; ncols];
    let mut remaining_content = avail_content;
    let mut remaining_unfixed = ncols;

    loop {
        if remaining_unfixed == 0 {
            break;
        }
        let share = remaining_content / remaining_unfixed;
        let to_fix: Vec<usize> = (0..ncols)
            .filter(|&i| !fixed[i] && naturals[i] <= share)
            .collect();
        if to_fix.is_empty() {
            break;
        }
        for i in to_fix {
            fixed[i] = true;
            widths[i] = naturals[i];
            remaining_content = remaining_content.saturating_sub(naturals[i]);
            remaining_unfixed -= 1;
        }
    }

    if let Some(base) = remaining_content.checked_div(remaining_unfixed) {
        let mut extra = remaining_content % remaining_unfixed;
        for w in widths
            .iter_mut()
            .zip(fixed.iter())
            .filter_map(|(w, &f)| (!f).then_some(w))
        {
            let mut col_w = base;
            if extra > 0 {
                col_w += 1;
                extra -= 1;
            }
            *w = col_w.max(1);
        }
    }
    widths
}

/// Word-wraps a flattened cell (as produced by [`flatten_cell`]) to
/// `width` display columns. Each span's text is split on spaces into word
/// units that keep that span's own style; words glue across a span
/// boundary when there's no space between them (so a multi-styled glued
/// run like `**bold**tail` still wraps as one word), and a bare single-
/// space span is treated as a break point rather than a word. A word wider
/// than `width` on its own is hard-split. An empty cell produces one empty
/// line.
fn wrap_cell(spans: &[Span<'static>], width: usize) -> WrappedCell {
    let width = width.max(1);

    let mut words: Vec<Vec<(String, Style)>> = Vec::new();
    let mut cur: Vec<(String, Style)> = Vec::new();
    let mut cur_has_content = false;
    for span in spans {
        let style = span.style;
        let content = span.content.as_ref();
        if content.is_empty() {
            continue;
        }
        for (i, part) in content.split(' ').enumerate() {
            if i > 0 && cur_has_content {
                words.push(std::mem::take(&mut cur));
                cur_has_content = false;
            }
            if !part.is_empty() {
                cur.push((part.to_string(), style));
                cur_has_content = true;
            }
        }
    }
    if cur_has_content {
        words.push(cur);
    }

    let mut lines: WrappedCell = Vec::new();
    let mut cur_line: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let mut has_content = false;
    for word in &words {
        let word_w: usize = word.iter().map(|(t, _)| t.width()).sum();
        if word_w <= width {
            let sep = usize::from(has_content);
            if has_content && cur_w + sep + word_w > width {
                lines.push((std::mem::take(&mut cur_line), cur_w));
                cur_w = 0;
                has_content = false;
            }
            if has_content {
                cur_line.push(Span::raw(" "));
                cur_w += 1;
            }
            for (t, s) in word {
                cur_line.push(Span::styled(t.clone(), *s));
            }
            cur_w += word_w;
            has_content = true;
        } else {
            if has_content {
                lines.push((std::mem::take(&mut cur_line), cur_w));
                cur_w = 0;
                has_content = false;
            }
            hard_split_word(word, width, &mut lines);
        }
    }
    if has_content {
        lines.push((cur_line, cur_w));
    }
    if lines.is_empty() {
        lines.push((Vec::new(), 0));
    }
    lines
}

/// Splits a single over-wide word into `width`-sized chunks, appending
/// each as its own line to `lines`. Adjacent same-style characters within
/// a chunk are coalesced into one span; a run's own style always follows
/// its characters even across a hard split.
fn hard_split_word(word: &[(String, Style)], width: usize, lines: &mut WrappedCell) {
    let mut chunk: Vec<Span<'static>> = Vec::new();
    let mut chunk_w = 0usize;
    for (text, style) in word {
        for ch in text.chars() {
            let cw = ch.width().unwrap_or(0);
            if chunk_w > 0 && chunk_w + cw > width {
                lines.push((std::mem::take(&mut chunk), chunk_w));
                chunk_w = 0;
            }
            match chunk.last_mut() {
                Some(last) if last.style == *style => {
                    let mut s = last.content.to_string();
                    s.push(ch);
                    *last = Span::styled(s, *style);
                }
                _ => chunk.push(Span::styled(ch.to_string(), *style)),
            }
            chunk_w += cw;
        }
    }
    if !chunk.is_empty() {
        lines.push((chunk, chunk_w));
    }
}

/// Pads a single cell line to `col_w` per `align`, appending the result to
/// `out`. Shared by the natural-width single-line row renderer and the
/// wrapped multi-line row renderer so alignment can't drift between them.
fn pad_cell(
    cell_spans: Vec<Span<'static>>,
    cell_w: usize,
    col_w: usize,
    align: Alignment,
    out: &mut Vec<Span<'static>>,
) {
    let pad = col_w.saturating_sub(cell_w);
    match align {
        Alignment::Right => {
            out.push(Span::raw(" ".repeat(pad)));
            out.extend(cell_spans);
        }
        Alignment::Center => {
            let l = pad / 2;
            let rgt = pad - l;
            out.push(Span::raw(" ".repeat(l)));
            out.extend(cell_spans);
            out.push(Span::raw(" ".repeat(rgt)));
        }
        _ => {
            out.extend(cell_spans);
            out.push(Span::raw(" ".repeat(pad)));
        }
    }
}

fn render_row(
    cells: &[CellSpans],
    widths: &[usize],
    aligns: &[Alignment],
    avail: usize,
    border: Style,
) -> Vec<Span<'static>> {
    let empty: CellSpans = (Vec::new(), 0);
    let mut spans = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for (i, col_w) in widths.iter().enumerate() {
        let (cell_spans, cell_w) = cells.get(i).unwrap_or(&empty);
        let sep_w = if i > 0 { 3 } else { 0 };
        if i > 0 && used + sep_w + col_w > avail {
            truncated = true;
            break;
        }
        if i > 0 {
            spans.push(Span::styled(" │ ", border));
        }
        pad_cell(
            cell_spans.clone(),
            *cell_w,
            *col_w,
            aligns.get(i).copied().unwrap_or(Alignment::None),
            &mut spans,
        );
        used += sep_w + col_w;
    }
    if truncated {
        spans.push(Span::raw("…"));
    }
    spans
}

/// Emits a table row's wrapped display lines: row height is the tallest
/// cell's wrapped-line count; line `k` shows each cell's `k`-th wrapped
/// line (blank past that cell's own height), padded/aligned via
/// [`pad_cell`] and separated by the same styled ` │ ` used elsewhere in
/// the table. Every emitted line maps to `src_line`.
fn render_wrapped_row(
    cells: &[CellSpans],
    widths: &[usize],
    aligns: &[Alignment],
    border: Style,
    src_line: usize,
    r: &mut Renderer,
) {
    let empty: CellSpans = (Vec::new(), 0);
    let empty_line: CellSpans = (Vec::new(), 0);
    let wrapped: Vec<WrappedCell> = widths
        .iter()
        .enumerate()
        .map(|(i, &w)| {
            let (spans, _) = cells.get(i).unwrap_or(&empty);
            wrap_cell(spans, w)
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for line_idx in 0..height {
        let mut spans = Vec::new();
        for (i, col_w) in widths.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" │ ", border));
            }
            let (line_spans, line_w) = wrapped[i].get(line_idx).unwrap_or(&empty_line);
            pad_cell(
                line_spans.clone(),
                *line_w,
                *col_w,
                aligns.get(i).copied().unwrap_or(Alignment::None),
                &mut spans,
            );
        }
        r.emit_line(spans, src_line);
    }
}

fn render_rule_row(widths: &[usize], avail: usize, border: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for (i, col_w) in widths.iter().enumerate() {
        let sep_w = if i > 0 { 3 } else { 0 };
        if i > 0 && used + sep_w + col_w > avail {
            truncated = true;
            break;
        }
        if i > 0 {
            spans.push(Span::styled("─┼─", border));
        }
        spans.push(Span::styled("─".repeat(*col_w), border));
        used += sep_w + col_w;
    }
    if truncated {
        spans.push(Span::raw("…"));
    }
    spans
}

/// Concatenated text of a display line's spans.
pub(crate) fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Slice a display line by an inclusive screen-cell range `[c0, c1]`,
/// keeping every char whose cell span intersects the range (terminal
/// selection convention: the cell under each endpoint is included).
/// Zero-width chars ride along with the char they attach to.
pub(crate) fn slice_cols(line: &Line, c0: usize, c1: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    let mut last_included = false;
    for span in &line.spans {
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if w == 0 {
                if last_included {
                    out.push(ch);
                }
                continue;
            }
            let included = col <= c1 && col + w > c0;
            if included {
                out.push(ch);
            }
            last_included = included;
            col += w;
        }
    }
    out
}

/// Text covered by a rendered-view mouse selection over `lines`: endpoints
/// are absolute `(display_line, column)` in either order, both inclusive.
/// Each line is right-trimmed (kills table/box padding), joined with `\n`.
pub(crate) fn selection_text(lines: &[Line], a: (usize, usize), b: (usize, usize)) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let last = lines.len() - 1;
    let (l0, l1) = (start.0.min(last), end.0.min(last));
    if l0 == l1 {
        // Tuple normalization already ordered the columns here.
        return slice_cols(&lines[l0], start.1, end.1)
            .trim_end()
            .to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(l1 - l0 + 1);
    out.push(
        slice_cols(&lines[l0], start.1, usize::MAX)
            .trim_end()
            .to_string(),
    );
    for line in &lines[l0 + 1..l1] {
        out.push(line_text(line).trim_end().to_string());
    }
    out.push(slice_cols(&lines[l1], 0, end.1).trim_end().to_string());
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn line_text(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn text(layout: &MdLayout) -> Vec<String> {
        layout.lines.iter().map(line_text).collect()
    }

    fn no_highlight(_token: &str, _body: &str) -> Vec<HlSpan> {
        Vec::new()
    }

    fn layout(src: &str, width: u16) -> MdLayout {
        let theme = Theme::rgb_dark();
        let mut hl = no_highlight;
        let mut ctx = MdContext {
            theme: &theme,
            highlight_code: &mut hl,
        };
        layout_markdown(src, width, 1, &mut ctx)
    }

    fn assert_src_lines_ok(l: &MdLayout, src: &str) {
        assert_eq!(l.lines.len(), l.source_lines.len());
        let total = src.lines().count().max(1);
        let mut prev = 0usize;
        for &s in &l.source_lines {
            assert!(s >= prev, "source_lines must be monotone non-decreasing");
            assert!(s < total, "source line {s} out of range (< {total})");
            prev = s;
        }
    }

    #[test]
    fn empty_input_is_zero_lines() {
        let l = layout("", 40);
        assert!(l.lines.is_empty());
        assert!(l.source_lines.is_empty());
    }

    #[test]
    fn whitespace_only_does_not_panic() {
        let l = layout("   \n\n\t\n", 40);
        assert!(l.lines.is_empty());
    }

    #[test]
    fn width_five_does_not_panic() {
        let l = layout("# Heading\n\nSome paragraph text here that is long.\n", 5);
        assert!(!l.lines.is_empty());
        assert_src_lines_ok(&l, "# Heading\n\nSome paragraph text here that is long.\n");
    }

    #[test]
    fn heading_strips_marker_and_uses_h1_style() {
        let l = layout("# Title", 40);
        assert_eq!(text(&l), vec!["Title".to_string()]);
        let theme = Theme::rgb_dark();
        assert_eq!(l.lines[0].spans[0].style, theme.md.h1);
    }

    #[test]
    fn bold_sets_modifier() {
        let l = layout("plain **bold** word", 40);
        let has_bold = l.lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold);
    }

    #[test]
    fn paragraph_wraps_without_splitting_words() {
        let src = "one two three four five six seven eight nine ten eleven twelve";
        let l = layout(src, 20);
        assert!(l.lines.len() > 1);
        for line in &l.lines {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= 20, "line {:?} exceeds width 20 ({w})", line_text(line));
        }
        let joined = text(&l).join(" ");
        for word in src.split_whitespace() {
            assert!(joined.contains(word));
        }
    }

    #[test]
    fn source_lines_track_mixed_document() {
        let src = "# Title\n\nA paragraph.\n\n- item one\n- item two\n\n> quoted\n";
        let l = layout(src, 40);
        assert_src_lines_ok(&l, src);
    }

    #[test]
    fn frontmatter_has_bar_prefix() {
        let src = "---\ntitle: hi\n---\n\nBody.\n";
        let l = layout(src, 40);
        assert!(text(&l)[0].starts_with("▌ "));
    }

    #[test]
    fn fenced_code_calls_highlight_with_token_and_body() {
        let theme = Theme::rgb_dark();
        let calls = std::cell::RefCell::new(Vec::new());
        let mut hl = |token: &str, body: &str| {
            calls
                .borrow_mut()
                .push((token.to_string(), body.to_string()));
            Vec::new()
        };
        let mut ctx = MdContext {
            theme: &theme,
            highlight_code: &mut hl,
        };
        let src = "```rust\nfn main() {}\n```\n";
        let _ = layout_markdown(src, 40, 1, &mut ctx);
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "rust");
        assert_eq!(calls[0].1, "fn main() {}");
    }

    #[test]
    fn code_block_line_truncated_with_ellipsis() {
        let src = "```\nthis is a very long code line that will not fit\n```\n";
        let l = layout(src, 20);
        let rendered = &text(&l)[0];
        assert!(rendered.ends_with('…'));
        let w: usize = l.lines[0].spans.iter().map(|s| s.content.width()).sum();
        assert!(w <= 20);
    }

    #[test]
    fn table_columns_padded_and_rule_present_and_truncated_when_overwide() {
        let src = "| a | bbbbbb |\n|---|---|\n| x | y |\n";
        let l = layout(src, 40);
        let lines = text(&l);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains('┼'));
        assert!(lines[0].contains("bbbbbb"));

        // Overwide-but-shrinkable: columns shrink and word-wrap instead of
        // truncating with `…`; every word still shows up somewhere in the
        // rendered output (individually — a wrapped word lands on its own
        // line, so whole multi-word cells won't appear as one contiguous
        // substring), and the table grows extra display lines.
        let wide = "| wordone wordtwo wordthree | wordfour wordfive wordsix \
                    | wordseven wordeight wordnine | wordten wordeleven wordtwelve |\n\
                    |---|---|---|---|\n\
                    | x | y | z | w |\n";
        let l2 = layout(wide, 60);
        let joined = text(&l2).join("\n");
        assert!(!joined.contains('…'));
        for word in [
            "wordone",
            "wordtwo",
            "wordthree",
            "wordfour",
            "wordfive",
            "wordsix",
            "wordseven",
            "wordeight",
            "wordnine",
            "wordten",
            "wordeleven",
            "wordtwelve",
        ] {
            assert!(joined.contains(word), "missing {word:?} in {joined:?}");
        }
        assert!(
            l2.lines.len() > 3,
            "expected wrapped rows to add display lines, got {}",
            l2.lines.len()
        );
    }

    #[test]
    fn table_narrow_natural_column_stays_natural_while_wide_ones_wrap() {
        // A narrow "✅" column keeps its natural width while two
        // paragraph-length columns shrink and wrap to fit.
        let sentence1 = "This paragraph contains quite a few different words strung \
                          together so the rendering engine is forced to wrap this \
                          particular table column across several separate lines \
                          without truncating any content.";
        let sentence2 = "Here is another sufficiently long sentence packed with extra \
                          descriptive words to guarantee this column also needs multiple \
                          wrapped lines when rendered inside the table cell region.";
        assert!(sentence1.len() > 100 && sentence2.len() > 100);
        let src = format!(
            "| ✅ | Description | Notes |\n|---|---|---|\n| ✅ | {sentence1} | {sentence2} |\n"
        );
        let l = layout(&src, 60);
        assert_src_lines_ok(&l, &src);

        let joined = text(&l).join(" ");
        for word in sentence1
            .split_whitespace()
            .chain(sentence2.split_whitespace())
        {
            assert!(joined.contains(word), "missing {word:?} in {joined:?}");
        }

        let mut widths = Vec::new();
        for line in &l.lines {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= 60, "line {:?} exceeds width 60 ({w})", line_text(line));
            widths.push(w);
        }
        // Rule row (contains '┼') has the same total display width as the
        // data lines around it.
        let rule_idx = text(&l).iter().position(|l| l.contains('┼')).unwrap();
        let data_idx = rule_idx + 1;
        assert_eq!(widths[rule_idx], widths[data_idx]);

        // Every source line the table's rows map to must fall within the
        // table's own 3-line source span (header/rule/data all on line 2).
        for &s in &l.source_lines {
            assert!(s <= 2, "source line {s} out of table's span");
        }
    }

    #[test]
    fn table_right_aligned_column_stays_right_aligned_when_wrapped() {
        let src = "| R |\n|---:|\n\
                    | wordalpha wordbeta wordgamma wordone wordtwo |\n";
        let l = layout(src, 20);
        let lines = text(&l);
        assert!(lines.len() > 3, "expected wrapping, got {lines:?}");

        let joined = lines.join(" ");
        for word in ["wordalpha", "wordbeta", "wordgamma", "wordone", "wordtwo"] {
            assert!(joined.contains(word));
        }

        for (i, line) in l.lines.iter().enumerate() {
            let rendered = &lines[i];
            if rendered.trim().is_empty() {
                continue;
            }
            // Right alignment pads on the left only: no trailing spaces,
            // and shorter-than-column lines have leading spaces.
            assert!(
                !rendered.ends_with(' '),
                "right-aligned line {rendered:?} has trailing padding"
            );
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= 20, "line {rendered:?} exceeds width 20 ({w})");
        }
    }

    #[test]
    fn table_single_word_longer_than_column_hard_splits() {
        let long_word = "a".repeat(38);
        let src = format!("| A | B |\n|---|---|\n| x | {long_word} |\n");
        let l = layout(&src, 20);
        let lines = text(&l);
        assert!(lines.len() > 3, "expected wrapping, got {lines:?}");
        for line in &l.lines {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= 20, "line exceeds width 20 ({w})");
        }
        let joined = lines.join("");
        assert!(!joined.contains('…'));
        // The long word's characters all made it into the output somewhere
        // (split across lines, but none dropped).
        assert_eq!(joined.chars().filter(|&c| c == 'a').count(), 38);
    }

    #[test]
    fn table_extreme_narrow_falls_back_to_truncation() {
        let src = "| aa | bb | cc | dd |\n|---|---|---|---|\n\
                    | aaaaaaaaaa | bbbbbbbbbb | cccccccccc | dddddddddd |\n";
        let l = layout(src, 12);
        assert!(!l.lines.is_empty());
        assert_src_lines_ok(&l, src);
        // 4 columns can't each get MIN_TABLE_COL at width 12, so this
        // falls back to the original single-line-per-row truncation path.
        assert_eq!(l.lines.len(), 3);
        assert!(text(&l)[0].ends_with('…'));
    }

    #[test]
    fn task_list_shows_done_and_todo_glyphs() {
        let src = "- [x] done\n- [ ] todo\n";
        let l = layout(src, 40);
        let joined = text(&l).join("\n");
        assert!(joined.contains('☑'));
        assert!(joined.contains('☐'));
    }

    #[test]
    fn nested_bullets_differ_by_depth() {
        let src = "- top\n  - nested\n    - deeper\n";
        let l = layout(src, 40);
        let joined = text(&l).join("\n");
        assert!(joined.contains('•'));
        assert!(joined.contains('◦'));
        assert!(joined.contains('▪'));
    }

    #[test]
    fn blockquote_has_bar_and_style() {
        let l = layout("> quoted text", 40);
        assert!(text(&l)[0].starts_with("▌ "));
        let theme = Theme::rgb_dark();
        let has_quote_color = l.lines[0]
            .spans
            .iter()
            .any(|s| s.style.fg == theme.md.blockquote.fg && s.style.fg != Some(Color::Reset));
        assert!(has_quote_color);
    }

    #[test]
    fn link_appends_url_when_it_differs_from_text() {
        let l = layout("[click here](https://example.com)", 60);
        let joined = text(&l).join("");
        assert!(joined.contains("click here"));
        assert!(joined.contains("https://example.com"));
    }

    #[test]
    fn autolink_has_no_suffix() {
        let l = layout("<https://example.com>", 60);
        let joined = text(&l).join("");
        assert_eq!(joined.matches("example.com").count(), 1);
    }

    #[test]
    fn inline_code_strips_backticks() {
        let l = layout("run `cargo test` now", 40);
        let joined = text(&l).join("");
        assert!(joined.contains("cargo test"));
        assert!(!joined.contains('`'));
    }

    #[test]
    fn horizontal_rule_spans_width() {
        let l = layout("---\n\ntext\n", 20);
        assert_eq!(text(&l)[0].chars().count(), 20);
    }

    #[test]
    fn one_blank_line_between_top_level_blocks_only() {
        let src = "# A\n\nPara one.\n\nPara two.\n";
        let l = layout(src, 40);
        let lines = text(&l);
        // heading, blank, para, blank, para = 5 lines
        assert_eq!(lines.len(), 5);
        assert!(lines[1].is_empty());
        assert!(lines[3].is_empty());
        assert!(!lines[0].is_empty());
        assert!(!lines.last().unwrap().is_empty());
    }

    #[test]
    fn footnote_reference_and_definition_render() {
        let src = "Body[^1].\n\n[^1]: Note text.\n";
        let l = layout(src, 40);
        let joined = text(&l).join("\n");
        assert!(joined.contains("[^1]"));
        assert!(joined.contains("Note text."));
    }

    #[test]
    fn mermaid_html_entities_decoded_and_label_quotes_unwrapped() {
        // Real-world labels are written for mermaid.js, where they are HTML:
        // entities must decode (a literal `&lt;` shatters the crate's lexer
        // into bogus extra nodes) and `["..."]` quotes must not display.
        let src = "```mermaid\nflowchart TD\n  d[\"deployments/&lt;env&gt;/<br/>provider config\"]\n  d --> c[\"done\"]\n```\n";
        let l = layout(src, 60);
        let joined = text(&l).join("\n");
        assert!(joined.contains("deployments/<env>/"));
        assert!(joined.contains("provider config"));
        assert!(!joined.contains("&lt"), "entities must be decoded");
        assert!(!joined.contains('"'), "label quotes must be stripped");
        // One node box, not a shattered parse: the label text appears once.
        assert_eq!(joined.matches("provider config").count(), 1);
    }

    #[test]
    fn mermaid_quoted_label_containing_closer_stays_quoted() {
        // `]` inside the label means unquoting would change the parse —
        // the preprocessor must leave it alone (and not panic).
        let src = "```mermaid\nflowchart TD\n  a[\"weird ] label\"] --> b[x]\n```\n";
        let l = layout(src, 60);
        assert_eq!(l.lines.len(), l.source_lines.len());
    }

    #[test]
    fn preprocess_decodes_amp_last() {
        assert_eq!(preprocess_mermaid("a & &amp; &lt;x&gt;"), "a & & <x>");
        assert_eq!(preprocess_mermaid("&amp;lt;"), "&lt;");
    }

    #[test]
    fn mermaid_flowchart_renders_as_diagram() {
        let src = "```mermaid\ngraph TD\n  A[Start] --> B[End]\n```\n";
        let l = layout(src, 40);
        let joined = text(&l).join("\n");
        assert!(joined.contains("Start"));
        assert!(joined.contains("End"));
        assert!(joined.contains('┌') || joined.contains('─') || joined.contains('│'));
        assert!(!text(&l)[0].starts_with("│ "));
    }

    #[test]
    fn mermaid_sequence_renders_participants() {
        let src = "```mermaid\nsequenceDiagram\n  participant Alice\n  participant Bob\n  Alice->>Bob: hi\n```\n";
        let l = layout(src, 40);
        let joined = text(&l).join("\n");
        assert!(joined.contains("Alice"));
        assert!(joined.contains("Bob"));
    }

    #[test]
    fn mermaid_render_failure_falls_back_to_code_block() {
        // "zzznotadiagram" is not a recognized diagram header, so
        // mermaid-text returns Err(UnsupportedDiagram) and rendering must
        // fall back to the plain gutter code block.
        let src = "```mermaid\nzzznotadiagram\n```\n";
        let l = layout(src, 40);
        assert!(text(&l)[0].starts_with("│ "));
        assert!(text(&l)[0].contains("zzznotadiagram"));
    }

    #[test]
    fn mermaid_source_lines_stay_monotone() {
        let src = "# Title\n\n```mermaid\ngraph TD\n  A[Start] --> B[End]\n```\n\nAfter.\n";
        let l = layout(src, 40);
        assert_src_lines_ok(&l, src);
    }

    #[test]
    fn mermaid_at_tiny_width_does_not_panic() {
        let src = "```mermaid\ngraph TD\n  A[Start] --> B[End]\n```\n";
        let l = layout(src, 15);
        assert!(!l.lines.is_empty());
    }

    #[test]
    fn mermaid_token_is_case_insensitive_but_exact_word() {
        let src = "```MERMAID\ngraph TD\n  A[Foo] --> B[Bar]\n```\n";
        let l = layout(src, 40);
        assert!(!text(&l)[0].starts_with("│ "));
        let joined = text(&l).join("\n");
        assert!(joined.contains("Foo"));
        assert!(joined.contains("Bar"));

        let src2 = "```mermaidjs\ngraph TD\n  A[Foo] --> B[Bar]\n```\n";
        let l2 = layout(src2, 40);
        assert!(text(&l2)[0].starts_with("│ "));
    }

    #[test]
    fn strikethrough_sets_modifier() {
        let l = layout("~~gone~~", 40);
        let has = l.lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(has);
    }

    // -----------------------------------------------------------------
    // Mouse-selection extraction: slice_cols / selection_text
    // -----------------------------------------------------------------

    #[test]
    fn slice_cols_middle_slice_on_plain_ascii() {
        let l = Line::from(vec![Span::raw("hello world")]);
        assert_eq!(slice_cols(&l, 6, 10), "world");
    }

    #[test]
    fn slice_cols_endpoints_are_inclusive() {
        let l = Line::from(vec![Span::raw("hello world")]);
        assert_eq!(slice_cols(&l, 0, 0), "h");
    }

    #[test]
    fn slice_cols_wide_char_included_by_either_cell() {
        // 日本語: each char occupies 2 screen cells (0-1, 2-3, 4-5).
        let l = Line::from(vec![Span::raw("日本語")]);
        // Range touches only the *second* cell of the first char but the
        // whole char must still come back.
        assert_eq!(slice_cols(&l, 1, 1), "日");
        // Same for the middle char's second cell only.
        assert_eq!(slice_cols(&l, 3, 3), "本");
    }

    #[test]
    fn slice_cols_crosses_span_boundary() {
        let l = Line::from(vec![
            Span::styled("foo", Style::default().fg(Color::Red)),
            Span::styled("bar", Style::default().fg(Color::Blue)),
        ]);
        // "foobar": cols 2..=4 -> 'o' (end of first span), 'b','a' (start
        // of second span).
        assert_eq!(slice_cols(&l, 2, 4), "oba");
    }

    #[test]
    fn slice_cols_out_of_range_returns_empty_without_panic() {
        let l = Line::from(vec![Span::raw("hello")]);
        assert_eq!(slice_cols(&l, 100, 200), "");
    }

    #[test]
    fn selection_text_single_line_trims_trailing_whitespace() {
        let lines = vec![Line::from(vec![Span::raw("hello   ")])];
        let got = selection_text(&lines, (0, 0), (0, 7));
        assert_eq!(got, "hello");
    }

    #[test]
    fn selection_text_multi_line_joins_and_trims_each_line() {
        let lines = vec![
            Line::from(vec![Span::raw("abcdefgh")]),
            Line::from(vec![Span::raw("middle line  ")]),
            Line::from(vec![Span::raw("xyz123456")]),
        ];
        let expected = "defgh\nmiddle line\nxyz12";
        // Forward order: anchor above head.
        assert_eq!(selection_text(&lines, (0, 3), (2, 4)), expected);
        // Reversed order: head above anchor — same result either way.
        assert_eq!(selection_text(&lines, (2, 4), (0, 3)), expected);
    }

    #[test]
    fn selection_text_empty_lines_is_empty_string() {
        let lines: Vec<Line> = Vec::new();
        assert_eq!(selection_text(&lines, (0, 0), (5, 5)), "");
    }

    #[test]
    fn selection_text_out_of_range_columns_no_panic() {
        let lines = vec![Line::from(vec![Span::raw("hi")])];
        assert_eq!(selection_text(&lines, (0, 50), (0, 60)), "");
    }
}
