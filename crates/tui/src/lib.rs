mod app;
mod buffers;
mod completion;
mod dispatch;
mod editor;
mod ex;
pub mod help;
mod input;
mod jumps;
mod lsp;
mod markdown;
mod picker;
mod recent;
mod render;
mod search;
pub mod testing;
mod theme;
mod util;

pub use app::run;
pub use editor::Editor;
pub use markdown::ViewMode;

pub(crate) use editor::{InsertOrigin, PendingInsert, Register};
