//! End-to-end smoke tests. One spawns rust-analyzer, opens a broken file,
//! and verifies that `publishDiagnostics` reports an error; one drives the
//! client against a scripted misbehaving server. Each is gated on its
//! binary being available so environments without it are a no-op.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vix_lsp::{path_to_uri, LspClient, ServerConfig, ServerEvent};

#[test]
fn rust_analyzer_reports_a_diagnostic() {
    let cfg = ServerConfig::rust_analyzer();
    if !cfg.available() {
        eprintln!("skipping: rust-analyzer not on PATH or not runnable");
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
            if let ServerEvent::Diagnostics {
                uri: u,
                diagnostics,
            } = ev
            {
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

/// A scripted stand-in for a language server: answers `initialize`, sends
/// one frame of garbage, answers one hover, then exits without being asked.
/// Covers what a real server only does on a bad day — a malformed message
/// must be skipped rather than kill the reader, and a server that dies must
/// surface as `Exited` so the editor can drop the client instead of timing
/// out on every later request.
const FAKE_SERVER: &str = r#"
import json, sys

def read():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if not line:
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":")[1])
    return json.loads(sys.stdin.buffer.read(length))

def write_raw(body):
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()

def write(msg):
    write_raw(json.dumps(msg).encode())

sys.stderr.buffer.write(b"not utf-8: \xff\xfe\n")
sys.stderr.buffer.flush()
while True:
    msg = read()
    if msg.get("method") == "initialize":
        write({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}})
    elif msg.get("method") == "textDocument/hover":
        write_raw(b"{ this is not json")
        write({"jsonrpc": "2.0", "id": msg["id"], "result": {"contents": "hi"}})
        sys.exit(0)
"#;

#[test]
fn client_survives_a_bad_frame_and_reports_server_exit() {
    let tmp = tempdir();
    let script = tmp.join("fake_server.py");
    std::fs::write(&script, FAKE_SERVER).unwrap();
    let cfg = ServerConfig {
        cmd: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        language_id: "plaintext".into(),
        probe_args: vec!["--version".into()],
    };
    if !cfg.available() {
        eprintln!("skipping: python3 not on PATH or not runnable");
        return;
    }

    let client = LspClient::start(cfg, &tmp).expect("spawn fake server");
    let uri = path_to_uri(&tmp.join("a.txt")).expect("uri");
    let id = client.hover(uri, 0, 0);
    let (result, error) = client
        .wait_response(id, Duration::from_secs(10))
        .expect("hover response arrives despite the malformed frame before it");
    assert!(error.is_none());
    assert_eq!(result.unwrap()["contents"], "hi");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while !exited && Instant::now() < deadline {
        match client.try_recv() {
            Some(ServerEvent::Exited) => exited = true,
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    std::fs::remove_dir_all(&tmp).ok();
    assert!(exited, "a server that exits on its own must be reported");
}

/// A scratch directory, unique per call: pid and timestamp separate runs,
/// the atomic counter separates calls within one run. Nanosecond stamps are
/// not unique across threads — two callers in the same instant would get one
/// shared directory and race on it.
fn tempdir() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = std::env::temp_dir();
    let name = format!(
        "vix-lsp-test-{}-{}-{n}",
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
