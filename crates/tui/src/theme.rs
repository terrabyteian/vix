//! Centralized color/style palette for the editor. Phase 4a routes every
//! hardcoded `Color`/`Style` value used by rendering through this struct so
//! future theming work has a single place to change values. `default_dark()`
//! reproduces the exact values that used to be scattered as literals across
//! `render/*.rs` and `picker/*.rs` — this pass is value-identical, zero
//! visual change.
use ratatui::style::{Color, Modifier, Style};

use vix_syntax::HIGHLIGHT_NAMES;

#[derive(Clone)]
pub(crate) struct Theme {
    // --- editor chrome ------------------------------------------------------
    /// Line-number gutter column.
    pub gutter: Style,
    /// `~` placeholder for lines past EOF.
    pub tilde: Style,
    /// Statusline background band (middle path segment + right info segment).
    pub statusline: Style,
    pub mode_normal: Style,
    pub mode_insert: Style,
    /// Shared by `Visual` and `VisualLine` — both render identically today.
    pub mode_visual: Style,
    pub mode_command: Style,

    // --- buffer overlays ------------------------------------------------------
    pub selection: Style,
    pub search_hl: Style,
    pub yank_flash: Style,
    /// Normal/Visual-mode block cursor (reverse-video).
    pub cursor_normal: Style,
    /// Insert-mode cursor (underline).
    pub cursor_insert: Style,

    // --- diagnostics ------------------------------------------------------
    pub diag_error: Style,
    pub diag_warn: Style,
    pub diag_info: Style,
    pub diag_hint: Style,
    /// Gutter glyph color when a line has no diagnostic.
    pub diag_none: Style,

    // --- popups (hover, completion) ------------------------------------------------------
    pub hover_bg: Style,
    pub hover_header: Style,
    /// Completion popup, unselected item.
    pub popup: Style,
    /// Completion popup, selected item.
    pub popup_selected: Style,

    // --- picker chrome ------------------------------------------------------
    /// Shared accent/border/dim tokens, reused as raw colors by call sites
    /// that compose their own one-off `Style`s (matches how these were
    /// consumed as bare `Color` consts before this pass).
    pub accent: Color,
    pub accent_hi: Color,
    pub border: Color,
    pub dim: Color,
    /// fzf-style query match highlight color (picker fullscreen list).
    pub match_hi: Color,
    /// Overlay-picker backdrop + default (unselected) row style.
    pub picker_bg: Style,
    /// Overlay/fullscreen picker header/prompt band.
    pub picker_header: Style,
    /// Overlay-picker detail (preview snippet) rows.
    pub picker_detail: Style,
    /// Selected list row, both overlay and fullscreen pickers.
    pub picker_selected: Style,
    /// Bright bold white text: fullscreen tab counts + prompt query text.
    pub picker_strong: Style,

    // --- syntax ------------------------------------------------------
    /// LUT indexed by position in `vix_syntax::HIGHLIGHT_NAMES`. `None` means
    /// "unstyled" (default foreground) — mirrors the old `scope_style`
    /// function's fallthrough.
    pub scope_styles: Vec<Option<Style>>,
}

impl Theme {
    pub(crate) fn default_dark() -> Self {
        Self {
            gutter: Style::default().fg(Color::DarkGray),
            tilde: Style::default().fg(Color::DarkGray),
            statusline: Style::default().bg(Color::DarkGray).fg(Color::White),
            mode_normal: Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            mode_insert: Style::default()
                .bg(Color::Green)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            mode_visual: Style::default()
                .bg(Color::Magenta)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            mode_command: Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),

            selection: Style::default().bg(Color::Blue).fg(Color::White),
            search_hl: Style::default().bg(Color::Yellow).fg(Color::Black),
            yank_flash: Style::default().bg(Color::LightYellow).fg(Color::Black),
            cursor_normal: Style::default().add_modifier(Modifier::REVERSED),
            cursor_insert: Style::default()
                .add_modifier(Modifier::UNDERLINED)
                .fg(Color::White),

            diag_error: Style::default().fg(Color::Red),
            diag_warn: Style::default().fg(Color::Yellow),
            diag_info: Style::default().fg(Color::Cyan),
            diag_hint: Style::default().fg(Color::Gray),
            diag_none: Style::default().fg(Color::Reset),

            hover_bg: Style::default().bg(Color::DarkGray).fg(Color::White),
            hover_header: Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            popup: Style::default().bg(Color::Rgb(30, 30, 40)).fg(Color::White),
            popup_selected: Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),

            accent: Color::Cyan,
            accent_hi: Color::LightCyan,
            border: Color::DarkGray,
            dim: Color::Gray,
            match_hi: Color::Yellow,
            picker_bg: Style::default().bg(Color::Black).fg(Color::White),
            picker_header: Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            picker_detail: Style::default().bg(Color::DarkGray).fg(Color::White),
            picker_selected: Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            picker_strong: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),

            scope_styles: HIGHLIGHT_NAMES
                .iter()
                .map(|name| scope_style_for(name))
                .collect(),
        }
    }
}

/// Exact port of the old `scope_style` string-matching logic, now run once
/// (per name) at `Theme` construction instead of per highlight span per
/// render.
fn scope_style_for(name: &str) -> Option<Style> {
    let color = if name.starts_with("keyword") {
        Color::Magenta
    } else if name.starts_with("function") {
        Color::LightBlue
    } else if name.starts_with("type") {
        Color::Cyan
    } else if name.starts_with("string") {
        Color::LightYellow
    } else if name.starts_with("constant") {
        Color::LightRed
    } else {
        match name {
            "comment" => Color::DarkGray,
            "attribute" | "constructor" => Color::LightMagenta,
            "namespace" | "label" => Color::Yellow,
            "property" => Color::LightCyan,
            "tag" => Color::LightGreen,
            _ => return None,
        }
    };
    Some(Style::default().fg(color))
}
