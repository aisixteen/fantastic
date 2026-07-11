//! The **active kernel** — which sub-kernel the chat + hub currently drive.
//!
//! The TUI is a kernel MANAGER over a uniform plane of **sub-kernels** (see
//! `subkernel.rs` + design `25_subkernels.md`): `★ self` / `cli` (the in-proc
//! manager, native, full local streaming) and folder-host / foreigner nodes reached
//! over the loopback gateway. Both are normalized behind [`ActiveKernel`] so the hub
//! (`reflect_tree`) and the chat (`dispatch`) don't care which one is live.
//!
//! Iter-1: the in-proc arm is the current behavior verbatim; the remote arm does
//! reflect + verbs (request→response over HTTP) with live token-streaming via a WS
//! mirror that feeds the same `brain_rx` inbox. The roadmap (`26_federation.md`) swaps
//! the remote transport for the `ws_bridge` connector.

use std::sync::Arc;

use fantastic_host::gateway::KernelHandle;
use fantastic_kernel::{AgentId, Kernel};

use crate::subkernel::{Role, SubKernel};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Which sub-kernel the chat + hub drive right now.
#[derive(Clone)]
pub enum ActiveKernel {
    /// The in-proc manager kernel — the native roles `★ self` / `cli`, full local
    /// streaming.
    InProc(Arc<Kernel>),
    /// A folder-host / foreigner over the loopback gateway (reflect + verbs, with live
    /// emits mirrored via `WsStream`). Constructed when you open such a node.
    Remote(RemoteKernel),
}

/// A sub-kernel reached over the gateway — an HTTP handle + its [`SubKernel`]
/// descriptor (name/role/guid) for the header + dedup. Iter-1 realization is the
/// loopback HTTP handle (`Conn::Http`); the roadmap swaps it for a bridge connector.
#[derive(Clone)]
pub struct RemoteKernel {
    pub handle: KernelHandle,
    /// The sub-kernel descriptor (its `name` is the header label; `role`/`guid` too).
    pub sub: SubKernel,
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

/// One node on the **Kernels** screen — a [`SubKernel`] on the uniform vis plane,
/// or the trailing `+ attach` action. `self`/`cli`/folder-host/foreigners all render
/// through the SAME `Node::Kernel` arm (web is not special-cased).
pub enum Node {
    /// A sub-kernel node (role + live/dormant status shown uniformly).
    Kernel(SubKernel),
    /// The trailing `+ attach` action (opens the folder browser).
    Attach,
}

impl Node {
    /// The first line shown on the node: `★ self` for the manager, the sub-kernel
    /// name otherwise, or the `+ attach` action.
    pub fn label(&self) -> String {
        match self {
            Node::Kernel(s) if s.role == Role::SelfManager => "★ self".to_string(),
            Node::Kernel(s) => s.name.clone(),
            Node::Attach => "+ attach".to_string(),
        }
    }
}

/// The Kernels-screen model: the row of sub-kernel nodes + the selection. `★ self`
/// is always first; `+ attach` is always last; `cli`, the folder-host, and any
/// foreigners sit between — all on one uniform plane.
pub struct KernelsView {
    pub nodes: Vec<Node>,
    pub sel: usize,
}

impl Default for KernelsView {
    fn default() -> Self {
        Self {
            nodes: vec![Node::Kernel(SubKernel::self_manager()), Node::Attach],
            sel: 0,
        }
    }
}

impl KernelsView {
    /// Rebuild the nodes from the discovered sub-kernels (self · cli · folder-host ·
    /// foreigners, in order) — `+ attach` always last — keeping the selection index
    /// in range. This is the `.fantastic`-driven discovery result (no registry).
    pub fn rebuild(&mut self, subs: Vec<SubKernel>) {
        let mut nodes: Vec<Node> = subs.into_iter().map(Node::Kernel).collect();
        nodes.push(Node::Attach);
        self.sel = self.sel.min(nodes.len().saturating_sub(1));
        self.nodes = nodes;
    }

    /// ← : the previous node (clamped).
    pub fn move_left(&mut self) {
        self.sel = self.sel.saturating_sub(1);
    }

    /// → : the next node (clamped).
    pub fn move_right(&mut self) {
        if self.sel + 1 < self.nodes.len() {
            self.sel += 1;
        }
    }

    /// The selected node (clamped into range).
    pub fn selected(&self) -> &Node {
        &self.nodes[self.sel.min(self.nodes.len().saturating_sub(1))]
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

    /// Live end-to-end for the CLI-as-sub-kernel: the CLI native-anchors its own
    /// `.fantastic/cli` (the `cli` node is `● live`); then spawning a real folder
    /// kernel makes the folder-host node flip `○ dormant → ● live`. Proves the
    /// initiator anchor + `.fantastic`-driven folder-host discovery over a real kernel.
    ///
    ///   cargo test -p fantastic-tui cli_anchor_and_folder_host_liveness -- --ignored
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "spawns a real fantastic_kernel daemon; needs a built binary"]
    async fn cli_anchor_and_folder_host_liveness() {
        use crate::subkernel::{Role, Status};
        use fantastic_host::gateway::{resolve_kernel_bin, Runtime, Workspace};

        if resolve_kernel_bin(Runtime::Rust).is_none() {
            eprintln!(
                "SKIP: no fantastic_kernel binary (set FANTASTIC_KERNEL_BIN or build lib/rust)"
            );
            return;
        }
        let dir = std::env::temp_dir().join(format!("ft-cli-anchor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // The CLI anchors its own sub-kernel namespace natively — folder-anchored, and
        // host-independent (no folder kernel spawned yet).
        let ns = fantastic_host::ensure_cli_subkernel(&dir).expect("cli anchor");
        assert!(ns.is_dir(), "cli namespace <cwd>/.fantastic/cli exists");
        let anchor = dir
            .join(".fantastic")
            .join("agents")
            .join("cli_loader")
            .join("agent.json");
        assert!(anchor.is_file(), "cli_loader anchor written natively");
        // The `cli` node is `● live` (initiator — the in-proc manager is its kernel).
        assert_eq!(SubKernel::cli(ns.clone()).status, Status::Live);

        // No folder-host yet → no node.
        assert!(
            crate::discover_folder_host(&dir, true).await.is_none(),
            "no folder-host before spawn"
        );

        // Spawn a real folder kernel → the folder-host node reflect-pings `● live`.
        let handle = Workspace::new(dir.clone())
            .spawn(Runtime::Rust)
            .await
            .expect("spawn folder kernel");
        let host = crate::discover_folder_host(&dir, true)
            .await
            .expect("folder-host discovered after spawn");
        let live = host.status == Status::Live;
        let role = host.role;

        // Clean up the daemon + temp dir regardless of the assertions.
        let _ = handle.send("core", json!({"type":"shutdown_kernel"})).await;
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            role,
            Role::FolderHost,
            "the cwd kernel is the folder-host role"
        );
        assert!(
            live,
            "folder-host flips ○→● (reflect-pinged live) after spawn"
        );
    }
}
