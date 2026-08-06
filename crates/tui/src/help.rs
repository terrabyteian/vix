//! Built-in help docs. Embedded at compile time so `:help <topic>` works on
//! a stripped release binary with no runtime files.

/// One help topic: short slug, friendly title, and the markdown body.
pub struct Topic {
    pub slug: &'static str,
    pub title: &'static str,
    pub body: &'static str,
}

const TESTING: Topic = Topic {
    slug: "testing",
    title: "Manual Testing Playbook",
    body: include_str!("../../../docs/MANUAL_TESTING.md"),
};

const MARKDOWN: Topic = Topic {
    slug: "markdown",
    title: "Markdown Rendered View",
    body: "\
# Markdown rendered view

`.md` files open in a richly-rendered view by default: headings, bold /\n\
italic / strikethrough, code blocks (syntax-highlighted where the fence's\n\
language is known), lists (including task lists), blockquotes, tables,\n\
links, images, and footnotes are laid out and styled instead of shown as\n\
raw source. This help buffer is itself rendered the same way.\n\
\n\
Fenced ```mermaid blocks render as Unicode box-drawing diagrams instead of\n\
a plain code block; if the diagram type isn't supported, they fall back to\n\
a normal code block.\n\
\n\
## Toggling\n\
\n\
| Trigger | Effect |\n\
|---|---|\n\
| `Space m` | Toggle between rendered and raw for the active buffer |\n\
| `:preview` / `:pv` | Switch to rendered (no-op with a message if already rendered) |\n\
| `:raw` | Switch to raw (no-op with a message if already raw) |\n\
\n\
`:preview` on a non-markdown buffer refuses with \"not a markdown buffer\"\n\
instead of switching.\n\
\n\
The view mode is per-buffer: it's stored with the buffer and survives\n\
`<Tab>` / `<S-Tab>` cycling and `:e`/`:b`/`:bn`/`:bp` switches.\n\
\n\
## Moving around in the rendered view\n\
\n\
The rendered view has no editable cursor; these keys scroll the display\n\
instead (Normal mode only):\n\
\n\
| Key | Effect |\n\
|---|---|\n\
| `j` / `k` / `Down` / `Up` | Scroll one display line |\n\
| `Ctrl-D` / `Ctrl-U` | Scroll half a page |\n\
| `Ctrl-F` / `Ctrl-B`, `PageDown` / `PageUp` | Scroll a full page |\n\
| `gg` / `G` | Jump to the top / bottom |\n\
| Mouse wheel | Scroll 3 display lines |\n\
| Mouse drag | Select text; copied on release (OSC 52) |\n\
\n\
`Space` (leader), `<Tab>` / `<S-Tab>` (buffer cycling), `Ctrl-P`, `:`, and\n\
`Esc` all keep their normal global meaning while the rendered view is up.\n\
\n\
## Editing drops to raw\n\
\n\
There's nothing to edit in the rendered view, so any key that implies an\n\
edit switches back to raw first:\n\
\n\
- `i` `I` `a` `A` `o` `O` switch to raw *and* replay the key, so you land\n\
  directly in Insert mode at the mapped position.\n\
- `x` `X` `d` `c` `s` `p` `P` `u` `.` `~` `>` `<` `r` `J` `v` `V` and\n\
  `Ctrl-R` switch to raw only, leaving you in Normal mode with a\n\
  \"-- raw markdown; Space m to re-render --\" message.\n\
- `/` and `?` show a hint that search works in the raw view.\n\
\n\
Switching to raw lands the cursor on the source line of whatever display\n\
line was at the top of the rendered view.\n\
\n\
## Statusline\n\
\n\
While a buffer is in the rendered view, the statusline shows a `PREVIEW`\n\
chip in place of the usual `NORMAL` mode indicator.\n\
\n\
## Size cap\n\
\n\
Files over 2 MB always open raw, and `Space m` / `:preview` on them\n\
refuses with \"markdown preview disabled (file > 2 MB)\" rather than\n\
rendering — laying out a document that size on every edit isn't worth the\n\
latency. Raw editing is unaffected.\n\
",
};

/// Registry of all help topics. Keep alphabetised by slug.
pub const TOPICS: &[Topic] = &[MARKDOWN, TESTING];

/// Look up a topic by slug. Slug matching is case-insensitive.
pub fn lookup(slug: &str) -> Option<&'static Topic> {
    let needle = slug.trim().to_ascii_lowercase();
    TOPICS.iter().find(|t| t.slug == needle)
}

/// The page shown by `:help` with no argument: an index of available topics.
pub fn index() -> String {
    let mut out = String::from(
        "# vix help\n\
        \n\
        Built-in help. Open a topic with `:help <slug>`.\n\
        \n\
        ## Topics\n\
        \n",
    );
    for t in TOPICS {
        out.push_str(&format!("- `:help {}` — {}\n", t.slug, t.title));
    }
    out.push_str(
        "\n\
        ---\n\
        \n\
        Help buffers are read-only scratch buffers. `:bd` closes them; `q` in\n\
        normal mode (when bound) does the same. Edits stay in memory and are\n\
        lost when the buffer is closed.\n",
    );
    out
}
