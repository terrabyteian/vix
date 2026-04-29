//! LSP client. Hand-rolled JSON-RPC over a child process, wrapped so the
//! synchronous TUI loop can poll it without awaiting.
//!
//! Architecture:
//! - [`LspClient::start`] spawns the server binary and a dedicated OS thread
//!   hosting a current-thread Tokio runtime. All socket / pipe I/O lives there.
//! - Public methods (`notify_*`, `request_*`, `try_recv`) talk to that thread
//!   over `tokio::sync::mpsc` (outbound) and `std::sync::mpsc` (inbound).
//! - Every request returns a [`RequestId`]. Responses are delivered as
//!   [`ServerEvent::Response`] with the same id, so the caller can match
//!   intent (hover vs. goto-definition) on its own.
//!
//! The crate is intentionally low-level: it surfaces raw `serde_json::Value`
//! for response bodies, and doesn't cache document text. The TUI layer owns
//! versioning and LSP-to-editor coordinate translation.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc as smpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use lsp_types::{
    CodeActionContext, CodeActionParams, CompletionParams, Diagnostic, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentFormattingParams,
    FormattingOptions, GotoDefinitionParams, HoverParams, PartialResultParams, Position,
    PublishDiagnosticsParams, Range, RenameParams, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
    VersionedTextDocumentIdentifier, WorkDoneProgressParams,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
pub use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc as ampsc;

pub use lsp_types;

/// Opaque JSON-RPC request id. Caller holds these to correlate
/// [`ServerEvent::Response`] back to intent.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub i64);

/// A language server we know how to launch.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// e.g. "rust-analyzer"
    pub cmd: String,
    pub args: Vec<String>,
    /// LSP `languageId` for `didOpen`.
    pub language_id: String,
}

impl ServerConfig {
    pub fn rust_analyzer() -> Self {
        Self {
            cmd: "rust-analyzer".into(),
            args: vec![],
            language_id: "rust".into(),
        }
    }
    pub fn pyright() -> Self {
        Self {
            cmd: "pyright-langserver".into(),
            args: vec!["--stdio".into()],
            language_id: "python".into(),
        }
    }
    pub fn typescript() -> Self {
        Self {
            cmd: "typescript-language-server".into(),
            args: vec!["--stdio".into()],
            language_id: "typescript".into(),
        }
    }
    pub fn gopls() -> Self {
        Self {
            cmd: "gopls".into(),
            args: vec![],
            language_id: "go".into(),
        }
    }

    /// True if the configured binary exists on `$PATH`.
    pub fn available(&self) -> bool {
        which_on_path(&self.cmd).is_some()
    }
}

/// Match a filename/extension to a configured server. None if we don't know
/// about that language.
pub fn server_for_path(path: &Path) -> Option<ServerConfig> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "rs" => Some(ServerConfig::rust_analyzer()),
        "py" => Some(ServerConfig::pyright()),
        "ts" | "tsx" | "js" | "jsx" => Some(ServerConfig::typescript()),
        "go" => Some(ServerConfig::gopls()),
        _ => None,
    }
}

/// Convert an absolute filesystem path to a `file://` URI suitable for LSP.
pub fn path_to_uri(path: &Path) -> Result<Uri> {
    use std::str::FromStr;
    if !path.is_absolute() {
        return Err(anyhow!(
            "path must be absolute for file:// URI: {}",
            path.display()
        ));
    }
    // Percent-encode minimally — space and non-ASCII. Good enough for local
    // paths in practice.
    let raw = path.to_string_lossy();
    let mut encoded = String::with_capacity(raw.len() + 7);
    encoded.push_str("file://");
    for ch in raw.chars() {
        match ch {
            c if c.is_ascii_alphanumeric() => encoded.push(c),
            '/' | '-' | '_' | '.' | '~' | ':' => encoded.push(ch),
            c => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).as_bytes() {
                    encoded.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    Uri::from_str(&encoded).with_context(|| format!("bad uri: {encoded}"))
}

/// Recover a filesystem path from a `file://` URI. Returns None if the URI
/// isn't a local-file URI.
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    Some(PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn which_on_path(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Events surfaced to the TUI. `Response` carries the raw body so the caller
/// can deserialize only the variants it cares about.
#[derive(Debug)]
pub enum ServerEvent {
    /// Server published diagnostics for a document.
    Diagnostics {
        uri: Uri,
        diagnostics: Vec<Diagnostic>,
    },
    /// Response to a prior request (identified by `id`). `result` is the
    /// raw `result` field; `error` is set if the server returned an error.
    Response {
        id: RequestId,
        result: Option<Value>,
        error: Option<String>,
    },
    /// Server-side log message (`window/logMessage`).
    Log { level: i64, message: String },
    /// Server-side user-facing message (`window/showMessage`).
    Show { level: i64, message: String },
    /// Server process exited. No more events will arrive.
    Exited,
}

/// Commands the I/O thread services.
enum OutMsg {
    Notify {
        method: &'static str,
        params: Value,
    },
    Request {
        id: i64,
        method: &'static str,
        params: Value,
    },
    Shutdown,
}

/// Thin handle on a running LSP server. Cloneable — all methods are
/// thread-safe because all coordination goes through mpsc channels.
pub struct LspClient {
    tx_out: ampsc::UnboundedSender<OutMsg>,
    rx_ev: smpsc::Receiver<ServerEvent>,
    /// Events that were pulled off `rx_ev` by a blocking `wait_response` but
    /// didn't match the id it was waiting on. These come out of `try_recv`
    /// ahead of any new events from the channel.
    holding: Mutex<VecDeque<ServerEvent>>,
    next_id: AtomicI64,
    config: ServerConfig,
    /// Root URI sent in `initialize`.
    #[allow(dead_code)]
    root: Uri,
}

impl LspClient {
    /// Spawn `config.cmd` and perform the `initialize` / `initialized`
    /// handshake. Blocks the calling thread until the server responds to
    /// `initialize` (or the spawn fails).
    pub fn start(config: ServerConfig, root: &Path) -> Result<Self> {
        if !config.available() {
            return Err(anyhow!(
                "language server binary not found on PATH: {}",
                config.cmd
            ));
        }

        let root_url: Uri = path_to_uri(root)?;

        let (tx_out, rx_out) = ampsc::unbounded_channel::<OutMsg>();
        let (tx_ev, rx_ev) = smpsc::channel::<ServerEvent>();

        // Initialize done signal: blocks `start` until the server responds.
        let (init_tx, init_rx) = smpsc::channel::<Result<()>>();

        let cfg_thread = config.clone();
        let root_thread = root_url.clone();
        std::thread::Builder::new()
            .name(format!("lsp-{}", config.cmd))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(async move {
                    if let Err(e) =
                        run_client(cfg_thread, root_thread, rx_out, tx_ev.clone(), init_tx).await
                    {
                        let _ = tx_ev.send(ServerEvent::Log {
                            level: 1,
                            message: format!("lsp loop error: {e:#}"),
                        });
                    }
                    let _ = tx_ev.send(ServerEvent::Exited);
                });
            })
            .context("spawn lsp thread")?;

        match init_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow!("lsp thread died before initialize")),
        }

        Ok(Self {
            tx_out,
            rx_ev,
            holding: Mutex::new(VecDeque::new()),
            next_id: AtomicI64::new(1),
            config,
            root: root_url,
        })
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Poll for a pending server event without blocking. Returns None when
    /// the queue is empty. Drain in a loop.
    pub fn try_recv(&self) -> Option<ServerEvent> {
        if let Ok(mut q) = self.holding.lock() {
            if let Some(ev) = q.pop_front() {
                return Some(ev);
            }
        }
        self.rx_ev.try_recv().ok()
    }

    /// Block up to `timeout` for the response to `id`. Any other events that
    /// arrive in the meantime are stashed so a later `try_recv` returns them.
    /// Returns `(Ok(result), err)` or None on timeout/server death.
    pub fn wait_response(
        &self,
        id: RequestId,
        timeout: Duration,
    ) -> Option<(Option<Value>, Option<String>)> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let ev = match self.rx_ev.recv_timeout(remaining) {
                Ok(ev) => ev,
                Err(_) => return None,
            };
            match ev {
                ServerEvent::Response {
                    id: rid,
                    result,
                    error,
                } if rid == id => {
                    return Some((result, error));
                }
                other => {
                    if let Ok(mut q) = self.holding.lock() {
                        q.push_back(other);
                    }
                }
            }
        }
    }

    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn notify<P: Serialize>(&self, method: &'static str, params: P) {
        let params = serde_json::to_value(params).unwrap_or(Value::Null);
        let _ = self.tx_out.send(OutMsg::Notify { method, params });
    }

    fn request<P: Serialize>(&self, method: &'static str, params: P) -> RequestId {
        let id = self.next_id();
        let params = serde_json::to_value(params).unwrap_or(Value::Null);
        let _ = self.tx_out.send(OutMsg::Request { id, method, params });
        RequestId(id)
    }

    pub fn did_open(&self, uri: Uri, version: i32, text: String) {
        self.notify(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: self.config.language_id.clone(),
                    version,
                    text,
                },
            },
        );
    }

    pub fn did_change_full(&self, uri: Uri, version: i32, text: String) {
        self.notify(
            "textDocument/didChange",
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri, version },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text,
                }],
            },
        );
    }

    pub fn did_close(&self, uri: Uri) {
        self.notify(
            "textDocument/didClose",
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri },
            },
        );
    }

    pub fn hover(&self, uri: Uri, line: u32, character: u32) -> RequestId {
        self.request(
            "textDocument/hover",
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position { line, character },
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )
    }

    pub fn definition(&self, uri: Uri, line: u32, character: u32) -> RequestId {
        self.request(
            "textDocument/definition",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position { line, character },
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        )
    }

    pub fn completion(&self, uri: Uri, line: u32, character: u32) -> RequestId {
        self.request(
            "textDocument/completion",
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position { line, character },
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
        )
    }

    pub fn code_action(&self, uri: Uri, range: Range, diagnostics: Vec<Diagnostic>) -> RequestId {
        self.request(
            "textDocument/codeAction",
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range,
                context: CodeActionContext {
                    diagnostics,
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        )
    }

    pub fn rename(&self, uri: Uri, line: u32, character: u32, new_name: String) -> RequestId {
        self.request(
            "textDocument/rename",
            RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position { line, character },
                },
                new_name,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )
    }

    pub fn execute_command(&self, command: String, arguments: Vec<Value>) -> RequestId {
        self.request(
            "workspace/executeCommand",
            lsp_types::ExecuteCommandParams {
                command,
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )
    }

    pub fn formatting(&self, uri: Uri, tab_size: u32, insert_spaces: bool) -> RequestId {
        self.request(
            "textDocument/formatting",
            DocumentFormattingParams {
                text_document: TextDocumentIdentifier { uri },
                options: FormattingOptions {
                    tab_size,
                    insert_spaces,
                    properties: Default::default(),
                    trim_trailing_whitespace: Some(true),
                    insert_final_newline: Some(true),
                    trim_final_newlines: Some(true),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )
    }

    /// Fire-and-forget shutdown: sends `shutdown` + `exit` and drops the
    /// handle. The server thread drains and exits when the child does.
    pub fn shutdown(self) {
        let _ = self.tx_out.send(OutMsg::Shutdown);
    }
}

/// Deserialize a Response.result body into a typed value. Returns None if
/// the result was null (common for nothing-to-say LSP responses).
pub fn parse_response<T: DeserializeOwned>(result: Option<Value>) -> Result<Option<T>> {
    match result {
        None | Some(Value::Null) => Ok(None),
        Some(v) => Ok(Some(serde_json::from_value(v)?)),
    }
}

async fn run_client(
    config: ServerConfig,
    root: Uri,
    mut rx_out: ampsc::UnboundedReceiver<OutMsg>,
    tx_ev: smpsc::Sender<ServerEvent>,
    init_tx: smpsc::Sender<Result<()>>,
) -> Result<()> {
    let mut child = Command::new(&config.cmd)
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", config.cmd))?;

    let stdin = child.stdin.take().context("no stdin")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    // Stderr → Log events (server diagnostics about itself).
    let tx_stderr = tx_ev.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = tx_stderr.send(ServerEvent::Log {
                level: 2,
                message: line,
            });
        }
    });

    let writer = Arc::new(tokio::sync::Mutex::new(stdin));

    // Send initialize first and wait for its response before unlocking the
    // public `start` call. The reader task streams responses back via
    // `init_done_tx` for this one id only; after that, everything flows
    // through `tx_ev`.
    let init_id: i64 = 0;
    send_request(&writer, init_id, "initialize", initialize_params(&root)).await?;

    // Reader task: framed JSON-RPC from stdout → ServerEvent.
    let tx_reader = tx_ev.clone();
    let (init_done_tx, mut init_done_rx) = ampsc::channel::<Result<()>>(1);
    tokio::spawn(async move {
        if let Err(e) = read_loop(stdout, tx_reader, init_id, init_done_tx).await {
            eprintln!("lsp reader: {e:#}");
        }
    });

    match init_done_rx.recv().await {
        Some(Ok(())) => {
            let _ = init_tx.send(Ok(()));
        }
        Some(Err(e)) => {
            let _ = init_tx.send(Err(e));
            let _ = child.kill().await;
            return Ok(());
        }
        None => {
            let _ = init_tx.send(Err(anyhow!("server exited before initialize reply")));
            let _ = child.kill().await;
            return Ok(());
        }
    }

    // Send `initialized` notification.
    send_notification(&writer, "initialized", json!({})).await?;

    // Main outbound loop: drain `rx_out`.
    while let Some(msg) = rx_out.recv().await {
        match msg {
            OutMsg::Notify { method, params } => {
                let _ = send_notification(&writer, method, params).await;
            }
            OutMsg::Request { id, method, params } => {
                let _ = send_request(&writer, id, method, params).await;
            }
            OutMsg::Shutdown => {
                let _ = send_request(&writer, -1, "shutdown", Value::Null).await;
                let _ = send_notification(&writer, "exit", Value::Null).await;
                break;
            }
        }
    }

    let _ = child.wait().await;
    Ok(())
}

fn initialize_params(root: &Uri) -> Value {
    // Raw JSON — avoids churn if lsp-types tweaks defaults across versions.
    json!({
        "processId": std::process::id(),
        "rootUri": root.as_str(),
        "capabilities": {
            "textDocument": {
                "synchronization": {
                    "didSave": false,
                    "willSave": false,
                    "willSaveWaitUntil": false,
                },
                "publishDiagnostics": { "relatedInformation": false },
                "hover": { "contentFormat": ["markdown", "plaintext"] },
                "definition": { "linkSupport": false },
                "completion": {
                    "completionItem": {
                        "snippetSupport": false,
                        "documentationFormat": ["plaintext"],
                        "insertReplaceSupport": false,
                        "resolveSupport": { "properties": ["detail", "documentation"] }
                    },
                    "contextSupport": false
                },
                "rename": { "prepareSupport": false },
                "formatting": { "dynamicRegistration": false },
                "codeAction": {
                    "codeActionLiteralSupport": {
                        "codeActionKind": {
                            "valueSet": [
                                "", "quickfix", "refactor", "refactor.extract",
                                "refactor.inline", "refactor.rewrite", "source",
                                "source.organizeImports"
                            ]
                        }
                    },
                    "isPreferredSupport": true,
                    "disabledSupport": false,
                    "dataSupport": false,
                    "resolveSupport": { "properties": ["edit"] }
                },
            },
            "workspace": {
                "configuration": false,
                "workspaceEdit": {
                    "documentChanges": true,
                    "resourceOperations": ["create", "rename", "delete"]
                }
            }
        },
        "clientInfo": { "name": "vix", "version": env!("CARGO_PKG_VERSION") },
    })
}

async fn send_request(
    writer: &Arc<tokio::sync::Mutex<ChildStdin>>,
    id: i64,
    method: &str,
    params: Value,
) -> Result<()> {
    let msg = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    send_frame(writer, msg).await
}

async fn send_notification(
    writer: &Arc<tokio::sync::Mutex<ChildStdin>>,
    method: &str,
    params: Value,
) -> Result<()> {
    let msg = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    send_frame(writer, msg).await
}

async fn send_frame(writer: &Arc<tokio::sync::Mutex<ChildStdin>>, msg: Value) -> Result<()> {
    let body = serde_json::to_vec(&msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut w = writer.lock().await;
    w.write_all(header.as_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

async fn read_loop(
    stdout: ChildStdout,
    tx_ev: smpsc::Sender<ServerEvent>,
    init_id: i64,
    init_done_tx: ampsc::Sender<Result<()>>,
) -> Result<()> {
    let mut reader = BufReader::new(stdout);
    let mut init_done_tx = Some(init_done_tx);
    loop {
        // Read headers.
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                // EOF.
                if let Some(tx) = init_done_tx.take() {
                    let _ = tx.send(Err(anyhow!("server closed stdout"))).await;
                }
                return Ok(());
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                content_length = rest.trim().parse().ok();
            }
        }
        let len = content_length.ok_or_else(|| anyhow!("missing Content-Length"))?;
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).await?;

        let msg: Value = serde_json::from_slice(&body)?;

        // Dispatch: request from server (has id+method, needs response),
        // notification (no id), or response (has id, no method).
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).map(String::from);
        match (id, method) {
            (Some(id_val), None) => {
                // Response. Route init's reply specially.
                let id_num = id_val.as_i64();
                if Some(init_id) == id_num {
                    if let Some(tx) = init_done_tx.take() {
                        let err = msg.get("error").map(|e| e.to_string());
                        match err {
                            Some(e) => {
                                let _ = tx.send(Err(anyhow!("initialize error: {e}"))).await;
                            }
                            None => {
                                let _ = tx.send(Ok(())).await;
                            }
                        }
                    }
                    continue;
                }
                if let Some(n) = id_num {
                    let result = msg.get("result").cloned();
                    let error = msg
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .map(String::from);
                    let _ = tx_ev.send(ServerEvent::Response {
                        id: RequestId(n),
                        result,
                        error,
                    });
                }
            }
            (_, Some(m)) => {
                // Notification or request from server. We ignore server→client
                // requests (register capability etc.) for now.
                match m.as_str() {
                    "textDocument/publishDiagnostics" => {
                        if let Some(params) = msg.get("params").cloned() {
                            if let Ok(p) =
                                serde_json::from_value::<PublishDiagnosticsParams>(params)
                            {
                                let _ = tx_ev.send(ServerEvent::Diagnostics {
                                    uri: p.uri,
                                    diagnostics: p.diagnostics,
                                });
                            }
                        }
                    }
                    "window/logMessage" => {
                        if let Some(p) = msg.get("params") {
                            let level = p.get("type").and_then(|v| v.as_i64()).unwrap_or(3);
                            let message = p
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let _ = tx_ev.send(ServerEvent::Log { level, message });
                        }
                    }
                    "window/showMessage" => {
                        if let Some(p) = msg.get("params") {
                            let level = p.get("type").and_then(|v| v.as_i64()).unwrap_or(3);
                            let message = p
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let _ = tx_ev.send(ServerEvent::Show { level, message });
                        }
                    }
                    _ => {
                        // Ignore. If this is a server→client request with an
                        // id, protocol purity says we should reply with a
                        // "MethodNotFound" error. In practice servers don't
                        // break if we don't.
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_errs() {
        let cfg = ServerConfig {
            cmd: "__definitely_not_a_real_binary__".into(),
            args: vec![],
            language_id: "rust".into(),
        };
        let err = LspClient::start(cfg, Path::new("/"))
            .err()
            .expect("should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("not found"), "got: {msg}");
    }

    #[test]
    fn server_for_path_known() {
        assert_eq!(
            server_for_path(Path::new("x.rs")).unwrap().language_id,
            "rust"
        );
        assert_eq!(
            server_for_path(Path::new("x.py")).unwrap().language_id,
            "python"
        );
        assert!(server_for_path(Path::new("x.unknown")).is_none());
    }
}
