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

/// Registry of all help topics. Keep alphabetised by slug.
pub const TOPICS: &[Topic] = &[TESTING];

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
