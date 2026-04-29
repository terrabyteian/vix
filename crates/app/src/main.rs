use anyhow::{Context, Result};
use std::env;
use std::path::Path;
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
            println!(
                "vix {VERSION} — slim vim-motion editor\n\n\
                 usage: vix [PATH]\n\n\
                 PATH may be:\n  \
                   omitted        open the file picker in the current directory\n  \
                   a directory    chdir into it and open the file picker\n  \
                   a file         open it (creates an empty buffer if it doesn't exist)"
            );
            return Ok(());
        }
        _ => {}
    }
    let (buffer, open_picker) = match first {
        None => (Buffer::empty(), true),
        Some(p) => {
            let path = Path::new(&p);
            if path.is_dir() {
                env::set_current_dir(path).with_context(|| format!("failed to chdir to {p}"))?;
                (Buffer::empty(), true)
            } else {
                (
                    Buffer::load(&p).with_context(|| format!("failed to load {p}"))?,
                    false,
                )
            }
        }
    };
    vix_tui::run(buffer, open_picker).context("tui loop failed")?;
    Ok(())
}
