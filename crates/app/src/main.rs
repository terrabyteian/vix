use anyhow::{Context, Result};
use std::env;
use vix_core::Buffer;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let first = args.next();
    match first.as_deref() {
        Some("--version" | "-V") => {
            println!("vix {VERSION}");
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!("vix {VERSION} — slim vim-motion editor\n\nusage: vix [FILE]");
            return Ok(());
        }
        _ => {}
    }
    let buffer = match first {
        Some(p) => Buffer::load(&p).with_context(|| format!("failed to load {p}"))?,
        None => Buffer::empty(),
    };
    vix_tui::run(buffer).context("tui loop failed")?;
    Ok(())
}
