//! The **active kernel** — which kernel the chat + hub currently drive.
//!
//! The TUI is a kernel MANAGER. It always holds the in-proc `★ RAM` manager kernel
//! (full local streaming), and can switch to a **folder kernel** reached over the
//! loopback gateway. Both are normalized behind [`ActiveKernel`] so the hub
//! (`reflect_tree`) and the chat (`dispatch`) don't care which one is live.
//!
//! Phase 1: the in-proc arm is the current behavior verbatim; the remote arm does
//! reflect + verbs (request→response over HTTP). Live token-streaming for a remote
//! kernel is Phase 2 (a WS mirror that feeds the same `brain_rx` inbox).

use std::path::PathBuf;
use std::sync::Arc;

use fantastic_host::gateway::{KernelHandle, Runtime};
use fantastic_kernel::{AgentId, Kernel};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Which kernel the chat + hub drive right now.
#[derive(Clone)]
pub enum ActiveKernel {
    /// The in-proc manager kernel (`★ RAM`) — full local streaming.
    InProc(Arc<Kernel>),
    // NOTE(kernel-switcher): the Kernels screen constructs this when you select a
    // folder kernel (Slice 4); the arms below are already implemented so the switch
    // is a one-line change there. `allow` drops when that lands.
    /// A folder kernel over the loopback gateway (reflect + verbs; Phase 2 streams).
    #[allow(dead_code)]
    Remote(RemoteKernel),
}

/// A folder kernel reached over the gateway — an HTTP handle + a display label.
#[derive(Clone)]
pub struct RemoteKernel {
    pub handle: KernelHandle,
    /// Short label for the header/squares (the folder basename).
    pub label: String,
}

/// A **live emit-stream subscription** to a remote folder kernel's `web_ws` surface.
///
/// This is the Phase-2 parity pipe: it connects to `ws://127.0.0.1:<port>/x/ws`,
/// asks the remote to `watch` an agent, and forwards each `{type:"event", payload}`
/// frame's `payload` into the TUI's `brain_tx` inbox — the SAME channel the in-proc
/// kernel feeds. So a remote agent's token/say/status/done stream renders through the
/// unchanged `brain_rx` → `on_event` path, with no renderer changes. Dropping the
/// `WsStream` aborts the socket task (unsubscribe-on-switch-away).
pub struct WsStream {
    /// Outbound `{type:"watch", src:"<id>"}` requests → the writer half of the task.
    watch_tx: mpsc::UnboundedSender<String>,
    task: JoinHandle<()>,
}

/// `http://127.0.0.1:PORT[/…]` → `ws://127.0.0.1:PORT/x/ws`. The web_ws path is a
/// wildcard `/{host_id}/ws`, so any placeholder host id (`x`) works. `None` if the
/// base URL carries no numeric port.
fn ws_url(base_url: &str) -> Option<String> {
    let port = base_url.rsplit(':').next()?.trim_end_matches('/');
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("ws://127.0.0.1:{port}/x/ws"))
}

/// Extract the emitted `payload` from a web_ws text frame, iff it's an `event`
/// frame (`{type:"event", payload:…}`). Non-event frames (replies, errors) → `None`.
fn event_payload(frame: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(frame).ok()?;
    if v.get("type").and_then(Value::as_str) == Some("event") {
        v.get("payload").cloned()
    } else {
        None
    }
}

impl WsStream {
    /// Connect to the remote kernel's WS surface (port parsed from `base_url`) and
    /// start forwarding its emitted events into `events` (the bounded brain inbox).
    /// `None` if the port can't be parsed or the socket won't open.
    pub async fn connect(base_url: &str, events: mpsc::Sender<Value>) -> Option<WsStream> {
        let url = ws_url(base_url)?;
        let (ws, _resp) = connect_async(&url).await.ok()?;
        let (mut sink, mut source) = ws.split();
        let (watch_tx, mut watch_rx) = mpsc::unbounded_channel::<String>();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Outbound watch requests → the socket.
                    req = watch_rx.recv() => match req {
                        Some(req) => {
                            if sink.send(Message::Text(req)).await.is_err() {
                                break;
                            }
                        }
                        None => break, // the WsStream was dropped
                    },
                    // Inbound: unwrap `{type:"event", payload}` → the brain inbox.
                    msg = source.next() => match msg {
                        Some(Ok(Message::Text(t))) => {
                            if let Some(payload) = event_payload(&t) {
                                if events.send(payload).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Some(Ok(_)) => {} // ignore ping/binary
                        _ => break,       // closed / error
                    },
                }
            }
        });
        Some(WsStream { watch_tx, task })
    }

    /// Subscribe to an agent's emits on the remote (idempotent server-side).
    pub fn watch(&self, id: &str) {
        let _ = self
            .watch_tx
            .send(json!({ "type": "watch", "src": id }).to_string());
    }
}

impl Drop for WsStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One square on the **Kernels** screen (the top nav level).
pub enum Square {
    /// The in-proc `★ RAM` manager kernel — always first, always live.
    Ram,
    /// A folder kernel from the persisted registry (live = a reflect-ping answered).
    Folder {
        dir: PathBuf,
        runtime: Runtime,
        live: bool,
    },
    /// The trailing `+ new` action (opens the folder browser).
    New,
}

impl Square {
    /// The folder basename shown on the square (`~` collapsed), or the fixed labels.
    pub fn label(&self) -> String {
        match self {
            Square::Ram => "★ RAM".to_string(),
            Square::New => "+ new".to_string(),
            Square::Folder { dir, .. } => dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string_lossy().into_owned()),
        }
    }
}

/// The Kernels-screen model: the row of squares + the selection. `★ RAM` is
/// synthetic (always index 0); `+ new` is always last; folder kernels sit between.
pub struct KernelsView {
    pub squares: Vec<Square>,
    pub sel: usize,
}

impl Default for KernelsView {
    fn default() -> Self {
        Self {
            squares: vec![Square::Ram, Square::New],
            sel: 0,
        }
    }
}

impl KernelsView {
    /// Rebuild the squares from the registry folders + their live flags — `★ RAM`
    /// first, `+ new` last — keeping the selection index in range.
    pub fn rebuild(&mut self, folders: Vec<(PathBuf, Runtime, bool)>) {
        let mut squares = vec![Square::Ram];
        for (dir, runtime, live) in folders {
            squares.push(Square::Folder { dir, runtime, live });
        }
        squares.push(Square::New);
        self.sel = self.sel.min(squares.len() - 1);
        self.squares = squares;
    }

    /// ← : the previous square (clamped).
    pub fn move_left(&mut self) {
        self.sel = self.sel.saturating_sub(1);
    }

    /// → : the next square (clamped).
    pub fn move_right(&mut self) {
        if self.sel + 1 < self.squares.len() {
            self.sel += 1;
        }
    }

    /// The selected square (clamped into range).
    pub fn selected(&self) -> &Square {
        &self.squares[self.sel.min(self.squares.len().saturating_sub(1))]
    }
}

impl ActiveKernel {
    /// Reflect the whole NESTED agent tree (the hub reads `reply["tree"]`).
    pub async fn reflect_tree(&self) -> Value {
        match self {
            ActiveKernel::InProc(k) => {
                k.send(&AgentId::from("kernel"), json!({"type":"reflect"}))
                    .await
            }
            // The gateway reflects the ROOT record (which carries `["tree"]`); pass
            // it straight through — `Hub::from_reflect` reads `["tree"]` and falls
            // back to a synthetic tree if it's absent.
            ActiveKernel::Remote(r) => r.handle.reflect(None).await.unwrap_or_else(|_| json!({})),
        }
    }

    /// The flat list of addressable agent ids (Tab-complete / Shift-Tab / nogo).
    pub async fn agent_ids(&self) -> Vec<String> {
        let tree = match self {
            ActiveKernel::InProc(k) => {
                k.send(
                    &AgentId::from("kernel"),
                    json!({"type":"reflect","tree":"ids"}),
                )
                .await
            }
            ActiveKernel::Remote(r) => r.handle.reflect(None).await.unwrap_or_else(|_| json!({})),
        };
        let mut ids = vec!["kernel".to_string()];
        crate::collect_ids(&tree, &mut ids);
        ids
    }

    /// Dispatch a verb to `target` and return the reply. Runs inside a spawned task.
    ///
    /// InProc **mirrors** the target's inbox first (`watch`) so any events it emits
    /// while handling the verb render live via `brain_rx` — the same path `@ai`
    /// uses. Remote is HTTP request→response (its live emits arrive over the Phase-2
    /// WS mirror, not here).
    pub async fn dispatch(&self, target: &AgentId, payload: Value) -> Value {
        match self {
            ActiveKernel::InProc(k) => {
                k.watch(target, AgentId::from(crate::CLIENT_ID)).await;
                k.send(target, payload).await
            }
            ActiveKernel::Remote(r) => r
                .handle
                .send(target.as_str(), payload)
                .await
                .unwrap_or_else(|e| json!({ "error": e.to_string() })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_maps_http_base_to_wildcard_ws() {
        assert_eq!(
            ws_url("http://127.0.0.1:8088").as_deref(),
            Some("ws://127.0.0.1:8088/x/ws")
        );
        assert_eq!(
            ws_url("http://127.0.0.1:54321/").as_deref(),
            Some("ws://127.0.0.1:54321/x/ws")
        );
        assert_eq!(ws_url("http://localhost").as_deref(), None); // no numeric port
        assert_eq!(ws_url("").as_deref(), None);
    }

    #[test]
    fn event_payload_unwraps_only_event_frames() {
        // An event frame → its raw payload (the same {type,source,…} the brain emits).
        let f = r#"{"type":"event","payload":{"type":"token","source":"brain","text":"hi"}}"#;
        let p = event_payload(f).expect("event payload");
        assert_eq!(p.get("type").and_then(Value::as_str), Some("token"));
        assert_eq!(p.get("source").and_then(Value::as_str), Some("brain"));
        // Replies / errors / malformed → None (they aren't emitted events).
        assert!(event_payload(r#"{"type":"reply","id":"1","data":{}}"#).is_none());
        assert!(event_payload(r#"{"type":"event"}"#).is_none()); // event, no payload
        assert!(event_payload("not json").is_none());
    }

    /// Live end-to-end: spawn a real folder kernel, open the WS mirror, `watch` the
    /// root, drive a `send` (the kernel fans the payload out to watchers), and assert
    /// the mirror forwards it as an event. Proves Part A over a real socket.
    ///
    ///   cargo test -p fantastic-tui ws_mirror_live -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "spawns a real fantastic_kernel daemon; needs a built binary"]
    async fn ws_mirror_live() {
        use fantastic_host::gateway::{resolve_kernel_bin, Runtime, Workspace};
        use std::time::Duration;

        if resolve_kernel_bin(Runtime::Rust).is_none() {
            eprintln!(
                "SKIP: no fantastic_kernel binary (set FANTASTIC_KERNEL_BIN or build lib/rust)"
            );
            return;
        }
        let dir = std::env::temp_dir().join(format!("ft-wsmirror-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let handle = Workspace::new(dir.clone())
            .spawn(Runtime::Rust)
            .await
            .expect("spawn folder kernel");

        let (tx, mut rx) = mpsc::channel::<Value>(64);
        let stream = WsStream::connect(&handle.base_url, tx)
            .await
            .expect("ws mirror connects to the live kernel");
        stream.watch("core");
        // Let the watch register, then drive a send — the kernel fans the raw payload
        // out to `core`'s watchers (send.rs `fanout_to_watchers`).
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = handle.send("core", json!({"type":"reflect"})).await;

        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;

        // Clean up the daemon + temp dir regardless of the assertion outcome.
        let _ = handle.send("core", json!({"type":"shutdown_kernel"})).await;
        let _ = std::fs::remove_dir_all(&dir);

        let ev = got
            .expect("an event should arrive within 3s")
            .expect("the mirror channel stays open");
        assert_eq!(
            ev.get("type").and_then(Value::as_str),
            Some("reflect"),
            "the mirror forwarded the fanned-out send payload: {ev}"
        );
    }
}
