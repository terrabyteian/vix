//! End-to-end smoke test: spawn rust-analyzer, open a broken file, and
//! verify that `publishDiagnostics` reports an error. Gated on the binary
//! being available so CI environments without it are a no-op.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vix_lsp::{path_to_uri, LspClient, ServerConfig, ServerEvent};

#[test]
fn rust_analyzer_reports_a_diagnostic() {
    let cfg = ServerConfig::rust_analyzer();
    if !cfg.available() {
        eprintln!("skipping: rust-analyzer not on PATH");
        return;
    }

    // Scratch crate with a type error.
    let tmp = tempdir();
    std::fs::write(
        tmp.join("Cargo.toml"),
        r#"[package]
name = "lsp-smoke"
version = "0.0.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    let main_src = "fn main() { let x: u32 = \"not a number\"; }\n";
    let main_path = tmp.join("src/main.rs");
    std::fs::write(&main_path, main_src).unwrap();

    let client = LspClient::start(cfg, &tmp).expect("spawn rust-analyzer");
    let uri = path_to_uri(&main_path).expect("uri");
    client.did_open(uri.clone(), 1, main_src.to_string());

    // Poll for diagnostics on our file. rust-analyzer takes several seconds
    // to initialize for a fresh crate; give it a generous window.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut got = false;
    while Instant::now() < deadline {
        while let Some(ev) = client.try_recv() {
            if let ServerEvent::Diagnostics { uri: u, diagnostics } = ev {
                if u == uri && !diagnostics.is_empty() {
                    got = true;
                    break;
                }
            }
        }
        if got {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    client.shutdown();
    std::fs::remove_dir_all(&tmp).ok();

    assert!(
        got,
        "rust-analyzer did not report a diagnostic within the deadline"
    );
}

fn tempdir() -> PathBuf {
    let base = std::env::temp_dir();
    let name = format!(
        "vix-lsp-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let p = base.join(name);
    std::fs::create_dir_all(&p).unwrap();
    p
}
