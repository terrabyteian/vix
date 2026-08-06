//! Centralized color/style palette for the editor.
//!
//! Three variants, selected once at startup by [`Theme::detect`]:
//! - **RGB dark** — a hand-tuned truecolor palette for terminals advertising
//!   24-bit support. Desaturated hues that read well on any dark background;
//!   the buffer background itself is never painted (`Color::Reset`), so the
//!   terminal's own background always shows through.
//! - **ANSI-16** — the classic named-color table. Used when truecolor isn't
//!   advertised, and on light backgrounds (the user's palette adapts the 16
//!   colors to their scheme; hardcoded RGB darks would not).
//! - **Monochrome** — `NO_COLOR` set: modifiers only (bold/dim/reverse/underline).
//!
//! Detection is env-only (`NO_COLOR`, `COLORTERM`, `COLORFGBG`) — vix stays
//! zero-config. `Editor::new` starts on the ANSI table so tests and headless
//! construction are environment-independent; `run()` swaps in the detected
//! theme before the first frame.
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
    /// Statusline mode chip for the rendered-markdown preview view.
    pub mode_preview: Style,

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
    /// that compose their own one-off `Style`s.
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
    /// Emphasized text: fullscreen tab counts + prompt query text.
    pub picker_strong: Style,

    // --- syntax ------------------------------------------------------
    /// LUT indexed by position in `vix_syntax::HIGHLIGHT_NAMES`. `None` means
    /// "unstyled" (default foreground).
    pub scope_styles: Vec<Option<Style>>,

    // --- markdown view ------------------------------------------------------
    pub md: MdStyles,
}

/// Styles for the rendered-markdown view.
#[derive(Clone)]
pub(crate) struct MdStyles {
    pub h1: Style,
    pub h2: Style,
    pub h3: Style,
    pub h4: Style,
    /// Inline `` `code` `` spans.
    pub code_inline: Style,
    /// The `│ ` bar left of fenced code blocks.
    pub code_gutter: Style,
    /// Quoted text.
    pub blockquote: Style,
    /// The `▌ ` bar left of blockquotes.
    pub blockquote_bar: Style,
    /// List bullets & ordered-list numbers.
    pub bullet: Style,
    /// `☑` task-list item.
    pub task_done: Style,
    /// `☐` task-list item.
    pub task_todo: Style,
    /// Link text.
    pub link: Style,
    /// Dim appended `(url)`.
    pub link_url: Style,
    /// Horizontal rule `─`.
    pub rule: Style,
    pub table_border: Style,
    pub table_header: Style,
    /// Dim YAML frontmatter block.
    pub frontmatter: Style,
}

impl MdStyles {
    fn rgb_dark() -> Self {
        use rgb::*;
        Self {
            h1: Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
            h2: Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
            h3: Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            h4: Style::default().fg(SKY).add_modifier(Modifier::BOLD),
            code_inline: Style::default().fg(PEACH).bg(SURFACE0),
            code_gutter: Style::default().fg(OVERLAY),
            blockquote: Style::default().fg(SUBTEXT).add_modifier(Modifier::ITALIC),
            blockquote_bar: Style::default().fg(GREEN),
            bullet: Style::default().fg(BLUE),
            task_done: Style::default().fg(GREEN),
            task_todo: Style::default().fg(OVERLAY),
            link: Style::default()
                .fg(SAPPHIRE)
                .add_modifier(Modifier::UNDERLINED),
            link_url: Style::default().fg(OVERLAY),
            rule: Style::default().fg(SURFACE1),
            table_border: Style::default().fg(SURFACE1),
            table_header: Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            frontmatter: Style::default().fg(OVERLAY),
        }
    }

    fn ansi() -> Self {
        Self {
            h1: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            h2: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            h3: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            h4: Style::default().fg(Color::Cyan),
            code_inline: Style::default().fg(Color::Yellow),
            code_gutter: Style::default().fg(Color::DarkGray),
            blockquote: Style::default().add_modifier(Modifier::ITALIC),
            blockquote_bar: Style::default().fg(Color::Green),
            bullet: Style::default().fg(Color::Blue),
            task_done: Style::default().fg(Color::Green),
            task_todo: Style::default().fg(Color::DarkGray),
            link: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
            link_url: Style::default().fg(Color::DarkGray),
            rule: Style::default().fg(Color::DarkGray),
            table_border: Style::default().fg(Color::DarkGray),
            table_header: Style::default().add_modifier(Modifier::BOLD),
            frontmatter: Style::default().fg(Color::DarkGray),
        }
    }

    fn monochrome() -> Self {
        let plain = Style::new();
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let dim = Style::default().add_modifier(Modifier::DIM);
        let underlined = Style::default().add_modifier(Modifier::UNDERLINED);
        Self {
            h1: bold.add_modifier(Modifier::UNDERLINED),
            h2: bold,
            h3: bold,
            h4: bold.add_modifier(Modifier::DIM),
            code_inline: dim,
            code_gutter: dim,
            blockquote: dim.add_modifier(Modifier::ITALIC),
            blockquote_bar: dim,
            bullet: plain,
            task_done: plain,
            task_todo: dim,
            link: underlined,
            link_url: dim,
            rule: dim,
            table_border: dim,
            table_header: bold,
            frontmatter: dim,
        }
    }
}

/// Which palette [`Theme::detect`] picked. Split out as a pure function of
/// the env values so it's unit-testable without touching process env.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThemeKind {
    RgbDark,
    Ansi,
    Monochrome,
}

pub(crate) fn detect_kind(
    no_color: Option<&str>,
    colorterm: Option<&str>,
    colorfgbg: Option<&str>,
) -> ThemeKind {
    // NO_COLOR (any non-empty value) wins: https://no-color.org
    if no_color.is_some_and(|v| !v.is_empty()) {
        return ThemeKind::Monochrome;
    }
    // COLORFGBG is "fg;bg" (sometimes "fg;default;bg"). A light background
    // (bg 7 or 15) means our hardcoded dark RGB values would clash — the
    // ANSI table adapts to the user's palette, so prefer it.
    if let Some(v) = colorfgbg {
        if let Some(bg) = v.split(';').next_back() {
            if matches!(bg, "7" | "15") {
                return ThemeKind::Ansi;
            }
        }
    }
    let truecolor = colorterm
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("truecolor") || v.contains("24bit")
        })
        .unwrap_or(false);
    if truecolor {
        ThemeKind::RgbDark
    } else {
        ThemeKind::Ansi
    }
}

// The truecolor palette. Desaturated pastels tuned for dark backgrounds
// (Catppuccin-Macchiato-adjacent). Named here once; used only by rgb_dark().
mod rgb {
    use ratatui::style::Color;
    pub const TEXT: Color = Color::Rgb(0xca, 0xd3, 0xf5);
    pub const SUBTEXT: Color = Color::Rgb(0xa5, 0xad, 0xcb);
    pub const OVERLAY: Color = Color::Rgb(0x6e, 0x73, 0x8d);
    pub const SURFACE1: Color = Color::Rgb(0x49, 0x4d, 0x64);
    pub const SURFACE0: Color = Color::Rgb(0x36, 0x3a, 0x4f);
    pub const BASE: Color = Color::Rgb(0x24, 0x27, 0x3a);
    pub const PINK: Color = Color::Rgb(0xf5, 0xbd, 0xe6);
    pub const MAUVE: Color = Color::Rgb(0xc6, 0xa0, 0xf6);
    pub const RED: Color = Color::Rgb(0xed, 0x87, 0x96);
    pub const PEACH: Color = Color::Rgb(0xf5, 0xa9, 0x7f);
    pub const YELLOW: Color = Color::Rgb(0xee, 0xd4, 0x9f);
    pub const GREEN: Color = Color::Rgb(0xa6, 0xda, 0x95);
    pub const TEAL: Color = Color::Rgb(0x8b, 0xd5, 0xca);
    pub const SKY: Color = Color::Rgb(0x91, 0xd7, 0xe3);
    pub const SAPPHIRE: Color = Color::Rgb(0x7d, 0xc4, 0xe4);
    pub const BLUE: Color = Color::Rgb(0x8a, 0xad, 0xf4);
    pub const LAVENDER: Color = Color::Rgb(0xb7, 0xbd, 0xf8);
}

impl Theme {
    /// Pick the palette from the environment. Called once from `run()`;
    /// everything else (tests, headless construction) stays on the
    /// environment-independent ANSI table from `default_dark()`.
    pub(crate) fn detect() -> Self {
        let no_color = std::env::var("NO_COLOR").ok();
        let colorterm = std::env::var("COLORTERM").ok();
        let colorfgbg = std::env::var("COLORFGBG").ok();
        match detect_kind(
            no_color.as_deref(),
            colorterm.as_deref(),
            colorfgbg.as_deref(),
        ) {
            ThemeKind::RgbDark => Self::rgb_dark(),
            ThemeKind::Ansi => Self::default_dark(),
            ThemeKind::Monochrome => Self::monochrome(),
        }
    }

    /// Hand-tuned truecolor dark palette. The buffer background is never
    /// painted — only fills for chrome (statusline, popups, picker) and
    /// overlays (selection, search) use explicit backgrounds.
    pub(crate) fn rgb_dark() -> Self {
        use rgb::*;
        let mode = |bg: Color| {
            Style::default()
                .bg(bg)
                .fg(BASE)
                .add_modifier(Modifier::BOLD)
        };
        Self {
            gutter: Style::default().fg(OVERLAY),
            tilde: Style::default().fg(SURFACE1),
            statusline: Style::default().bg(SURFACE0).fg(TEXT),
            mode_normal: mode(BLUE),
            mode_insert: mode(GREEN),
            mode_visual: mode(MAUVE),
            mode_command: mode(PEACH),
            mode_preview: mode(TEAL),

            selection: Style::default().bg(SURFACE1),
            search_hl: Style::default().bg(YELLOW).fg(BASE),
            yank_flash: Style::default().bg(PEACH).fg(BASE),
            cursor_normal: Style::default().add_modifier(Modifier::REVERSED),
            cursor_insert: Style::default().add_modifier(Modifier::UNDERLINED).fg(TEXT),

            diag_error: Style::default().fg(RED),
            diag_warn: Style::default().fg(YELLOW),
            diag_info: Style::default().fg(BLUE),
            diag_hint: Style::default().fg(OVERLAY),
            diag_none: Style::default().fg(Color::Reset),

            hover_bg: Style::default().bg(SURFACE0).fg(TEXT),
            hover_header: Style::default()
                .bg(SURFACE0)
                .fg(BLUE)
                .add_modifier(Modifier::BOLD),
            popup: Style::default().bg(SURFACE0).fg(TEXT),
            popup_selected: Style::default()
                .bg(BLUE)
                .fg(BASE)
                .add_modifier(Modifier::BOLD),

            accent: BLUE,
            accent_hi: LAVENDER,
            border: SURFACE1,
            dim: OVERLAY,
            match_hi: YELLOW,
            picker_bg: Style::default().bg(BASE).fg(TEXT),
            picker_header: Style::default()
                .bg(SURFACE0)
                .fg(TEXT)
                .add_modifier(Modifier::BOLD),
            picker_detail: Style::default().bg(SURFACE0).fg(SUBTEXT),
            picker_selected: Style::default()
                .bg(SURFACE1)
                .fg(TEXT)
                .add_modifier(Modifier::BOLD),
            picker_strong: Style::default().fg(TEXT).add_modifier(Modifier::BOLD),

            scope_styles: HIGHLIGHT_NAMES
                .iter()
                .map(|name| rgb_scope_style_for(name))
                .collect(),
            md: MdStyles::rgb_dark(),
        }
    }

    /// The classic ANSI-16 table (the pre-4b look). Adapts to the user's
    /// terminal palette; used on non-truecolor terminals and light
    /// backgrounds, and as the environment-independent default for tests.
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
            mode_preview: Style::default()
                .bg(Color::Cyan)
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
                .map(|name| ansi_scope_style_for(name))
                .collect(),
            md: MdStyles::ansi(),
        }
    }

    /// `NO_COLOR`: no colors at all, structure carried by modifiers only.
    /// Reverse-video is reserved for the cursor and selections so they stay
    /// distinguishable; search uses underline, emphasis uses bold/dim.
    pub(crate) fn monochrome() -> Self {
        let plain = Style::default();
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let dim = Style::default().add_modifier(Modifier::DIM);
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let rev_bold = Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD);
        Self {
            gutter: dim,
            tilde: dim,
            statusline: rev,
            mode_normal: rev_bold,
            mode_insert: rev_bold,
            mode_visual: rev_bold,
            mode_command: rev_bold,
            mode_preview: rev_bold,

            selection: rev,
            search_hl: Style::default().add_modifier(Modifier::UNDERLINED),
            yank_flash: rev,
            cursor_normal: rev,
            cursor_insert: Style::default().add_modifier(Modifier::UNDERLINED),

            diag_error: bold,
            diag_warn: plain,
            diag_info: dim,
            diag_hint: dim,
            diag_none: plain,

            hover_bg: rev,
            hover_header: rev_bold,
            popup: rev,
            popup_selected: rev_bold,

            accent: Color::Reset,
            accent_hi: Color::Reset,
            border: Color::Reset,
            dim: Color::Reset,
            match_hi: Color::Reset,
            picker_bg: plain,
            picker_header: rev_bold,
            picker_detail: dim,
            picker_selected: rev_bold,
            picker_strong: bold,

            scope_styles: HIGHLIGHT_NAMES
                .iter()
                .map(|name| match *name {
                    "comment" => Some(dim),
                    n if n.starts_with("keyword") => Some(bold),
                    _ => None,
                })
                .collect(),
            md: MdStyles::monochrome(),
        }
    }
}

/// ANSI scope mapping — the original pre-theme `scope_style` logic, run once
/// per name at construction.
fn ansi_scope_style_for(name: &str) -> Option<Style> {
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

/// Truecolor scope mapping. Same scope→role assignments as the ANSI table,
/// with hues from the curated palette.
fn rgb_scope_style_for(name: &str) -> Option<Style> {
    use rgb::*;
    let color = if name.starts_with("keyword") {
        MAUVE
    } else if name.starts_with("function") {
        BLUE
    } else if name.starts_with("type") {
        TEAL
    } else if name.starts_with("string") {
        GREEN
    } else if name.starts_with("constant") {
        PEACH
    } else {
        match name {
            "comment" => OVERLAY,
            "attribute" | "constructor" => PINK,
            "namespace" | "label" => YELLOW,
            "property" => SKY,
            "tag" => SAPPHIRE,
            _ => return None,
        }
    };
    Some(Style::default().fg(color))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_wins_over_truecolor() {
        assert_eq!(
            detect_kind(Some("1"), Some("truecolor"), None),
            ThemeKind::Monochrome
        );
    }

    #[test]
    fn empty_no_color_is_ignored() {
        assert_eq!(
            detect_kind(Some(""), Some("truecolor"), None),
            ThemeKind::RgbDark
        );
    }

    #[test]
    fn truecolor_detected_case_insensitive() {
        assert_eq!(
            detect_kind(None, Some("TRUECOLOR"), None),
            ThemeKind::RgbDark
        );
        assert_eq!(detect_kind(None, Some("24bit"), None), ThemeKind::RgbDark);
    }

    #[test]
    fn plain_terminal_falls_back_to_ansi() {
        assert_eq!(detect_kind(None, None, None), ThemeKind::Ansi);
        assert_eq!(detect_kind(None, Some("yes"), None), ThemeKind::Ansi);
    }

    #[test]
    fn light_background_prefers_ansi_even_under_truecolor() {
        assert_eq!(
            detect_kind(None, Some("truecolor"), Some("0;15")),
            ThemeKind::Ansi
        );
        assert_eq!(
            detect_kind(None, Some("truecolor"), Some("0;default;7")),
            ThemeKind::Ansi
        );
        // Dark background: truecolor palette applies.
        assert_eq!(
            detect_kind(None, Some("truecolor"), Some("15;0")),
            ThemeKind::RgbDark
        );
    }

    #[test]
    fn all_variants_cover_every_scope_slot() {
        for theme in [
            Theme::rgb_dark(),
            Theme::default_dark(),
            Theme::monochrome(),
        ] {
            assert_eq!(theme.scope_styles.len(), HIGHLIGHT_NAMES.len());
        }
    }

    #[test]
    fn all_variants_construct_markdown_styles() {
        for theme in [
            Theme::rgb_dark(),
            Theme::default_dark(),
            Theme::monochrome(),
        ] {
            // Touch every field so a rename/removal doesn't silently drop
            // one of them from construction.
            let md = &theme.md;
            let _ = (
                md.h1,
                md.h2,
                md.h3,
                md.h4,
                md.code_inline,
                md.code_gutter,
                md.blockquote,
                md.blockquote_bar,
                md.bullet,
                md.task_done,
                md.task_todo,
                md.link,
                md.link_url,
                md.rule,
                md.table_border,
                md.table_header,
                md.frontmatter,
            );
            let _ = theme.mode_preview;
        }

        // Monochrome carries no color at all, including in the new
        // markdown styles.
        let mono = Theme::monochrome();
        for style in [
            mono.md.h1,
            mono.md.h2,
            mono.md.h3,
            mono.md.h4,
            mono.md.code_inline,
            mono.md.code_gutter,
            mono.md.blockquote,
            mono.md.blockquote_bar,
            mono.md.bullet,
            mono.md.task_done,
            mono.md.task_todo,
            mono.md.link,
            mono.md.link_url,
            mono.md.rule,
            mono.md.table_border,
            mono.md.table_header,
            mono.md.frontmatter,
        ] {
            assert_eq!(style.fg, None);
            assert_eq!(style.bg, None);
        }
        assert_eq!(mono.mode_preview.fg, None);
    }
}
