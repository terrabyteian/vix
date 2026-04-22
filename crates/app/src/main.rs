use anyhow::{Context, Result};
use std::env;
use vix_core::Buffer;

fn main() -> Result<()> {
    let path = env::args().nth(1);
    let buffer = match path {
        Some(p) => Buffer::load(&p).with_context(|| format!("failed to load {p}"))?,
        None => Buffer::empty(),
    };
    vix_tui::run(buffer).context("tui loop failed")?;
    Ok(())
}
