/// Editor mode. Cursor-first: visual mode is an ephemeral selection region
/// that operators consume, not a persistent state as in Helix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual,       // charwise visual
    VisualLine,   // V
    Command,      // `:` ex command entry
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
            Mode::Command => "COMMAND",
        }
    }
}

/// Operator awaiting a motion — e.g. after `d`, we're in operator-pending state
/// waiting for `w`, `iw`, `$`, etc. to know what range to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOp {
    Delete,
    Change,
    Yank,
    ShiftLeft,
    ShiftRight,
    Format,
    ToLower,
    ToUpper,
    SwapCase,
    ToggleComment,
}
