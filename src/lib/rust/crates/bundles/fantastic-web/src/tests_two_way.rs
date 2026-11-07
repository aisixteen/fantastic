//! THE TWO-WAY KERNEL BRIDGE (issue #476): kernel A's `ws_bridge` dials kernel
//! B's `web_ws` and says `hello {guid, leg}`; from then on B calls A's agents as
//! `<A guid>|<agent>` and watches them, gated by A's leg (its `ingress_rule`, its
//! `can_call`, the target's `callable_by`). The twin of python
//! `web_ws/tests/test_two_way.py`.
//!
//! Every case runs on BOTH transports: `memory` (A's leg on an in-process half
//! whose far end the test pipes into a real socket on B) and `ws` (A's leg dials
//! B itself, through a test-owned TCP forwarder that stands in for the ssh `-L`
//! tunnel, so the test can drop the link under it).

use super::binary_steps::{create, op, serve, tag, until};
use super::*;
use async_trait::async_trait;
use fantastic_ai_core::agent_loop::BackendConfig;
use fantastic_ai_core::provider::{Provider, ProviderError, ProviderEvent, ProviderStream};
use fantastic_bridge::transport::{BridgeTransport, Frame};
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::test_support::register_root_loader;
use fantastic_kernel::{Blob, Outcome};
use futures_util::{SinkExt, StreamExt};
use std::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as TMessage;

const A: &str = "A-g";
const VALUE: &str = "sk-A-STORE-41f7c2";
const REF: &str = "near/main";

// ─── the agents on A ─────────────────────────────────────────────────────────

/// `echo` answers `{echo: text}`; `hold` never answers (a call in flight when the
/// link drops) and says so on `held`.
struct Probe {
    held: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "two_way_probe.tools"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match payload["type"].as_str().unwrap_or("") {
            "reflect" => Ok(Some(json!({"id": id.0, "sentence": "probe", "verbs": {}}))),
            "echo" => Ok(Some(json!({"echo": payload["text"]}))),
            "hold" => {
                self.held.notify_one();
                std::future::pending::<()>().await;
                unreachable!("a pending future never completes")
            }
            _ => Ok(None),
        }
    }
}

/// A scripted model: one text part, then `done`, a few characters per token.
struct Canned;

#[async_trait]
impl Provider for Canned {
    async fn chat(&self, _m: &[Value], _max: Option<i64>) -> Result<ProviderStream, ProviderError> {
        let toks = ["Hel", "lo th", "ere ", "<call name=\"done\"></call>"];
        let evs: Vec<_> = toks
            .iter()
            .map(|t| Ok(ProviderEvent::Token((*t).to_string())))
            .collect();
        Ok(Box::pin(futures_util::stream::iter(evs)))
    }
    fn model(&self) -> String {
        "canned".into()
    }
}

const CFG: BackendConfig = BackendConfig {
    name: "canned_ai",
    model_window: fantastic_ai_core::constants::DEFAULT_CONTEXT_WINDOW,
    provider_cache: false,
};

/// The real ai_core verb table over [`Canned`].
struct CannedAi;

#[async_trait]
impl Bundle for CannedAi {
    fn name(&self) -> &str {
        "canned_ai.tools"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        k: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        if payload["type"] == json!("reflect") {
            return Ok(Some(
                json!({"id": id.0, "sentence": "canned AI", "verbs": {"send": ""}}),
            ));
        }
        let out = fantastic_ai_core::verbs::handle_common(id, payload, k, CFG, |_| {
            Arc::new(Canned) as Arc<dyn Provider>
        })
        .await?;
        Ok(Some(out.unwrap_or_else(|| json!({"reason": "unhandled"}))))
    }
    async fn on_delete(&self, id: &AgentId, k: &Arc<Kernel>) -> Result<(), BundleError> {
        fantastic_ai_core::verbs::on_delete(id, k).await;
        Ok(())
    }
}

// ─── the two kernels ─────────────────────────────────────────────────────────

/// A started kernel with `guid`: every leg, the memory store its rows and its AI
/// conversations land in, the probe and the scripted AI bundles.
async fn kernel(guid: &str, ai: &str, held: &Arc<tokio::sync::Notify>) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    reg.register(HANDLER_MODULE, WebBundle);
    reg.register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    reg.register(
        fantastic_bridge::WS_HANDLER_MODULE,
        fantastic_bridge::WsBridgeBundle,
    );
    reg.register(
        fantastic_credential_store::HANDLER_MODULE,
        fantastic_credential_store::test_support::bundle(),
    );
    reg.register(
        "two_way_probe.tools",
        Probe {
            held: Arc::clone(held),
        },
    );
    reg.register("canned_ai.tools", CannedAi);
    let k = bootstrap(reg, BootstrapOptions::in_memory())
        .expect("boot")
        .kernel;
    let root = k.root().expect("root").id.clone();
    let mut m = Map::new();
    m.insert("guid".to_string(), json!(guid));
    k.update(&root, m);
    let made = create(
        &k,
        json!({"handler_module": fantastic_ram_store::HANDLER_MODULE, "id": "memory",
               "callable_by": {ai: "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!("memory"), "{made}");
    let mut m = Map::new();
    m.insert("memory_store_id".to_string(), json!("memory"));
    k.update(&root, m);
    let out = k.start(false).await;
    assert_eq!(out["started"], json!(true), "{out}");
    k
}

#[derive(Clone, Copy, Debug)]
enum Wire {
    Memory,
    Ws,
}

const WIRES: [Wire; 2] = [Wire::Memory, Wire::Ws];

/// The test's ssh `-L`: a TCP forwarder on `[::1]:<B's port>` (the same port, so
/// the Host B sees is its own loopback authority) into B on `127.0.0.1`. `cut`
/// drops every live connection; the listener stays, so a redial gets through.
struct Tunnel {
    conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    accept: tokio::task::JoinHandle<()>,
}

impl Tunnel {
    async fn open(port: u16) -> Self {
        let listener = tokio::net::TcpListener::bind(("::1", port))
            .await
            .expect("bind the tunnel");
        let conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();
        let live = Arc::clone(&conns);
        let accept = tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let h = tokio::spawn(async move {
                    let mut out = tokio::net::TcpStream::connect(("127.0.0.1", port))
                        .await
                        .expect("tunnel to B");
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut out).await;
                });
                live.lock().unwrap().push(h);
            }
        });
        Self { conns, accept }
    }

    fn cut(&self) {
        for h in self.conns.lock().unwrap().drain(..) {
            h.abort();
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.cut();
        self.accept.abort();
    }
}

/// A memory half piped into a real socket on B: every frame crosses the codec.
/// `cut` drops the socket AND the half — the link is gone on both ends.
struct Pipe {
    half: Arc<dyn BridgeTransport>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Pipe {
    async fn open(port: u16, web: &str, half: Arc<dyn BridgeTransport>) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web}/ws"))
            .await
            .expect("ws connect");
        let (mut sink, mut source) = ws.split();
        let down = Arc::clone(&half);
        let inbound = tokio::spawn(async move {
            while let Some(Ok(msg)) = source.next().await {
                let _ = match msg {
                    TMessage::Text(t) => {
                        down.send_frame(serde_json::from_str(&t).expect("json"))
                            .await
                    }
                    TMessage::Binary(b) => {
                        let f = fantastic_io_bridge::codec::decode_binary_frame(&b).expect("frame");
                        match f.path {
                            Some(p) => down.send_binary(f.head, Blob::new(p, f.body)).await,
                            None => down.send_frame(f.head).await,
                        }
                    }
                    TMessage::Close(_) => break,
                    _ => Ok(()),
                };
            }
            down.close().await;
        });
        let up = Arc::clone(&half);
        let outbound = tokio::spawn(async move {
            while let Ok(f) = up.recv_frame().await {
                let m = match f {
                    Frame::Text(v) => TMessage::Text(fantastic_io_bridge::codec::encode_frame(&v)),
                    Frame::Binary(h, b) => TMessage::Binary(
                        fantastic_io_bridge::codec::encode_binary_frame(&h, &b.path, &b.bytes),
                    ),
                };
                if sink.send(m).await.is_err() {
                    return;
                }
            }
            let _ = sink.close().await;
        });
        Self {
            half,
            tasks: vec![inbound, outbound],
        }
    }

    async fn cut(&self) {
        for t in &self.tasks {
            t.abort();
        }
        self.half.close().await;
    }
}

/// The link under A's leg — what a test drops.
enum Link {
    Memory(Mutex<Option<Pipe>>),
    Ws(Tunnel),
}

/// Kernel A with its leg dialed into kernel B's `web_ws`.
struct Pair {
    wire: Wire,
    ka: Arc<Kernel>,
    kb: Arc<Kernel>,
    port: u16,
    b_web: String,
    b_ws: String,
    leg: String,
    link: Link,
    held: Arc<tokio::sync::Notify>,
    /// The pair's tag: A's agent `x` is `<tag>_x`.
    tag: String,
}

/// Stage B (a web with an open `web_ws` leg) and A (`probe`, the scripted AI
/// `ai`, a credential store `creds`, and the leg). `open` opens the leg's ingress;
/// `granted` lets the leg call / watch every agent above and has each accept it.
async fn pair(wire: Wire, open: bool, granted: bool) -> Pair {
    let held = Arc::new(tokio::sync::Notify::new());
    let t = tag();
    let kb = kernel("B-g", &format!("{t}_ai"), &held).await;
    let (b_web, b_ws, leg) = (format!("{t}_web"), format!("{t}_ws"), format!("{t}_a"));
    let port = serve(
        &kb,
        &b_web,
        &[json!({"handler_module": "web_ws.tools", "id": b_ws,
                 "callable_by": {"$parent": ["get_routes"]},
                 "ingress_rule": "allow_all_ingress",
                 "can_call": {"$parent": ["watch"], "probe": "*"}})],
    )
    .await;
    let made = create(
        &kb,
        json!({"handler_module": "two_way_probe.tools", "id": "probe",
               "callable_by": {b_ws.as_str(): "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!("probe"), "{made}");

    let ka = kernel(A, &format!("{t}_ai"), &held).await;
    // process-wide agent state (an AI's turn table) is keyed by id: every pair
    // names its agents on A apart
    let (ai, probe, creds) = (
        format!("{t}_ai"),
        format!("{t}_probe"),
        format!("{t}_creds"),
    );
    let accept = |granted: bool| {
        if granted {
            json!({leg.as_str(): "*"})
        } else {
            json!({})
        }
    };
    for rec in [
        json!({"handler_module": "two_way_probe.tools", "id": probe,
               "callable_by": accept(granted)}),
        json!({"handler_module": "canned_ai.tools", "id": ai, "model": "canned",
               "context_window": 32768, "memory_store_id": "memory",
               "can_call": {"memory": "*", "cli": "*"},
               "callable_by": accept(granted)}),
        json!({"handler_module": fantastic_credential_store::HANDLER_MODULE, "id": creds,
               "backend": "keychain", "refs": {REF: {"host": "api.example.com"}},
               "callable_by": accept(granted)}),
    ] {
        let id = rec["id"].clone();
        let made = create(&ka, rec).await;
        assert_eq!(made["id"], id, "{made}");
    }
    let mut rec = json!({"handler_module": fantastic_bridge::WS_HANDLER_MODULE, "id": leg,
                         "peer_id": b_web});
    match wire {
        Wire::Memory => rec["transport"] = json!("memory"),
        Wire::Ws => {
            rec["transport"] = json!("ws");
            rec["host"] = json!("[::1]");
            rec["port"] = json!(port);
        }
    }
    if open {
        rec["ingress_rule"] = json!("allow_all_ingress");
    }
    if granted {
        rec["can_call"] = json!({probe.as_str(): "*", ai.as_str(): "*", creds.as_str(): "*"});
    }
    let made = create(&ka, rec).await;
    assert_eq!(made["id"], json!(leg), "{made}");
    let link = match wire {
        Wire::Memory => Link::Memory(Mutex::new(None)),
        Wire::Ws => Link::Ws(Tunnel::open(port).await),
    };
    let p = Pair {
        wire,
        ka,
        kb,
        port,
        b_web,
        b_ws,
        leg,
        link,
        held,
        tag: t,
    };
    p.dial().await;
    p
}

impl Pair {
    /// The id of A's agent `name` (`ai`, `probe`, `creds`).
    fn n(&self, name: &str) -> String {
        format!("{}_{name}", self.tag)
    }

    /// `<A guid>|<A's agent name>`.
    fn remote(&self, name: &str) -> AgentId {
        AgentId::from(format!("{A}|{}", self.n(name)))
    }

    /// Boot A's leg (the first dial, or a redial after a drop): it says hello,
    /// and B routes A's guid through it.
    async fn dial(&self) {
        let leg = AgentId::from(self.leg.as_str());
        let half = matches!(self.wire, Wire::Memory).then(|| fantastic_bridge::inject_one(&leg));
        let booted = op(&self.ka, &self.leg, json!({"type": "boot"}))
            .await
            .result;
        assert_eq!(booted["booted"], json!(true), "{:?}: {booted}", self.wire);
        if let (Link::Memory(slot), Some(half)) = (&self.link, half) {
            let pipe = Pipe::open(self.port, &self.b_web, half).await;
            *slot.lock().unwrap() = Some(pipe);
        }
        until("B routes A's guid", || self.kb.peer_routes(A).len() == 1).await;
    }

    /// Drop the link under A's leg, then wait until both ends know.
    async fn cut(&self) {
        match &self.link {
            Link::Memory(slot) => {
                let pipe = slot.lock().unwrap().take().expect("a live pipe");
                pipe.cut().await;
            }
            Link::Ws(t) => t.cut(),
        }
        until("B forgot the dropped leg", || {
            self.kb.peer_routes(A).is_empty()
        })
        .await;
        let end = tokio::time::Instant::now() + Duration::from_secs(3);
        while self.reflect().await["connected"] == json!(true) {
            assert!(
                tokio::time::Instant::now() < end,
                "A's leg never saw the drop"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn reflect(&self) -> Value {
        op(&self.ka, &self.leg, json!({"type": "reflect"}))
            .await
            .result
    }

    /// B calls `agent` on A, as B's operator.
    async fn call(&self, agent: &str, payload: Value) -> Outcome {
        op(&self.kb, self.remote(agent).as_str(), payload).await
    }

    fn patch_leg(&self, key: &str, value: Value) {
        let mut m = Map::new();
        m.insert(key.to_string(), value);
        self.ka.update(&AgentId::from(self.leg.as_str()), m);
    }

    async fn has_watchers(&self, agent: &str) -> bool {
        self.ka.has_watchers(&AgentId::from(self.n(agent))).await
    }

    async fn until_watched(&self, agent: &str, on: bool) {
        let end = tokio::time::Instant::now() + Duration::from_secs(3);
        while self.has_watchers(agent).await != on {
            assert!(
                tokio::time::Instant::now() < end,
                "{:?}: {agent} watched never became {on}",
                self.wire
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn stop(&self) {
        let _ = op(
            &self.ka,
            "kernel",
            json!({"type": "delete_agent", "id": self.leg}),
        )
        .await;
        if let Link::Memory(slot) = &self.link {
            if let Some(p) = slot.lock().unwrap().take() {
                for t in &p.tasks {
                    t.abort();
                }
            }
        }
        stop(&self.kb, &self.b_web).await;
    }
}

/// A turn on A's AI for a non-cli caller — its stream goes to the AI's inbox, so
/// its watchers see it.
fn turn(text: &str) -> Value {
    json!({"type": "send", "text": text, "client_id": "remote"})
}

/// Everything waiting in `reader`'s inbox now.
fn drain(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(item) = rx.try_recv() {
        out.push(item.payload);
    }
    out
}

/// The kinds of a stream, `status:<phase>` for a status.
fn kinds(evs: &[Value]) -> Vec<String> {
    evs.iter()
        .map(|e| {
            let t = e["type"].as_str().unwrap_or("").to_string();
            if t == "status" {
                format!("status:{}", e["phase"].as_str().unwrap_or(""))
            } else {
                t
            }
        })
        .collect()
}

/// A full turn's stream as a watcher sees it: status phases, the text part
/// streamed, all before `done`.
fn assert_stream_before_done(evs: &[Value], wire: Wire) {
    let k = kinds(evs);
    let done = k
        .iter()
        .position(|t| t == "status:done")
        .unwrap_or_else(|| panic!("{wire:?}: no done in {k:?}"));
    let before = &k[..done];
    assert!(
        before.iter().any(|t| t.starts_with("status:")),
        "{wire:?}: no status before done: {k:?}"
    );
    for part in ["part_start", "part_delta", "part_end"] {
        assert!(
            before.iter().any(|t| t == part),
            "{wire:?}: no {part} before done: {k:?}"
        );
    }
}

async fn raw_ws(port: u16, web: &str) -> TestWs {
    tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web}/ws"))
        .await
        .expect("ws connect")
        .0
}

async fn next_of(ws: &mut TestWs, ftype: &str) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame in time")
            .expect("stream open")
            .expect("frame ok");
        if let TMessage::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).expect("json frame");
            if v["type"] == ftype {
                return v;
            }
        }
    }
}

// ─── hello ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_makes_a_kernel_leg_and_a_plain_client_stays_a_client() {
    for wire in WIRES {
        let p = pair(wire, true, true).await;
        assert_eq!(
            p.kb.peer_rows(),
            json!([{"guid": A, "via": p.b_ws}]),
            "{wire:?}"
        );
        // a connection that says no hello is the canvas page's: it calls, it is
        // answered, it is no address
        let mut page = raw_ws(p.port, &p.b_web).await;
        page.send(TMessage::Text(
            json!({"type": "call", "id": "c1", "target": "probe",
                   "payload": {"type": "echo", "text": "page"}})
            .to_string(),
        ))
        .await
        .unwrap();
        let reply = next_of(&mut page, "reply").await;
        assert_eq!(reply["data"], json!({"echo": "page"}), "{wire:?}");
        assert_eq!(p.kb.peer_rows().as_array().unwrap().len(), 1, "{wire:?}");
        // and B reaches A's agent through the leg
        let out = p.call("probe", json!({"type": "echo", "text": "hi"})).await;
        assert_eq!(out.result, json!({"echo": "hi"}), "{wire:?}");
        p.stop().await;
    }
}

// ─── calls B → A, gated as A's leg ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_from_b_is_gated_by_the_legs_seal_and_grants() {
    for wire in WIRES {
        let p = pair(wire, false, false).await;
        let echo = json!({"type": "echo", "text": "x"});
        // sealed by default: the teaching denial
        let sealed = p.call("probe", echo.clone()).await.result;
        assert_eq!(
            sealed["reason"],
            json!("unauthorized"),
            "{wire:?}: {sealed}"
        );
        assert!(sealed.get("hint").is_some(), "{wire:?}: {sealed}");
        // opened, but no grant: the leg may not call the probe
        p.patch_leg("ingress_rule", json!("allow_all_ingress"));
        let ungranted = p.call("probe", echo.clone()).await.result;
        assert_eq!(
            ungranted["reason"],
            json!("unauthorized"),
            "{wire:?}: {ungranted}"
        );
        assert_eq!(ungranted["side"], json!("egress"), "{wire:?}: {ungranted}");
        // granted both ends: it runs
        p.patch_leg("can_call", json!({p.n("probe"): "*"}));
        let mut m = Map::new();
        m.insert("callable_by".into(), json!({p.leg.as_str(): "*"}));
        p.ka.update(&AgentId::from(p.n("probe")), m);
        let ok = p.call("probe", echo.clone()).await.result;
        assert_eq!(ok, json!({"echo": "x"}), "{wire:?}");
        // sealed again: refused again, live
        p.patch_leg("ingress_rule", json!("deny_ingress"));
        let resealed = p.call("probe", echo).await.result;
        assert_eq!(
            resealed["reason"],
            json!("unauthorized"),
            "{wire:?}: {resealed}"
        );
        p.stop().await;
    }
}

// ─── watches B → A, events A → B ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn b_watches_an_ai_on_a_and_gets_its_stream_until_unwatch_or_drop() {
    for wire in WIRES {
        let p = pair(wire, true, true).await;
        let target = p.remote("ai");
        let w = AgentId::from("watcher");
        let mut rx = p.kb.ensure_inbox(&w);
        assert_eq!(p.kb.watch(&target, w.clone()).await, None, "{wire:?}");
        p.until_watched("ai", true).await;

        let out = p.call("ai", turn("one")).await;
        assert_eq!(
            out.status.as_str(),
            "completed",
            "{wire:?}: {:?}",
            out.result
        );
        assert_eq!(out.result["client_id"], json!("remote"), "{wire:?}");
        // the reply rode the same socket AFTER the turn's events
        assert_stream_before_done(&drain(&mut rx), wire);

        // unwatched: A stops streaming, the next turn brings nothing
        p.kb.unwatch(&target, &w).await;
        p.until_watched("ai", false).await;
        let out = p.call("ai", turn("two")).await;
        assert_eq!(out.status.as_str(), "completed", "{wire:?}");
        assert_eq!(drain(&mut rx), Vec::<Value>::new(), "{wire:?}");

        // watched again, then the link drops: A gives the watch back
        assert_eq!(p.kb.watch(&target, w.clone()).await, None, "{wire:?}");
        p.until_watched("ai", true).await;
        p.cut().await;
        p.until_watched("ai", false).await;
        let local = op(&p.ka, &p.n("ai"), turn("three")).await;
        assert_eq!(local.status.as_str(), "completed", "{wire:?}");
        assert_eq!(drain(&mut rx), Vec::<Value>::new(), "{wire:?}");
        p.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watch_from_b_is_gated_by_the_legs_watch_grant() {
    for wire in WIRES {
        let p = pair(wire, true, false).await;
        let target = p.remote("ai");
        let w = AgentId::from("watcher");
        let mut rx = p.kb.ensure_inbox(&w);
        // B's own side takes it (B's operator); A's leg holds no `watch` on `ai`
        assert_eq!(p.kb.watch(&target, w.clone()).await, None, "{wire:?}");
        // the leg serves calls but not watches: the turn runs, nothing streams
        p.patch_leg("can_call", json!({p.n("ai"): ["send"]}));
        let mut m = Map::new();
        m.insert("callable_by".into(), json!({p.leg.as_str(): "*"}));
        p.ka.update(&AgentId::from(p.n("ai")), m);
        let out = p.call("ai", turn("one")).await;
        assert_eq!(
            out.status.as_str(),
            "completed",
            "{wire:?}: {:?}",
            out.result
        );
        assert!(!p.has_watchers("ai").await, "{wire:?}");
        assert_eq!(drain(&mut rx), Vec::<Value>::new(), "{wire:?}");
        p.stop().await;
    }
}

// ─── secret marks ────────────────────────────────────────────────────────────

/// Everything kernel `k` keeps of its calls: the rows, the ledger, the state
/// stream and, after a flush, the memory store — as one text.
async fn everything_kept(k: &Arc<Kernel>, events: &Mutex<Vec<Value>>) -> String {
    k.flush_executions().await;
    let dumped = op(k, "memory", json!({"type": "dump"})).await.result;
    format!(
        "{}\n{}\n{:?}\n{}",
        serde_json::to_string(&k.rows.all()).unwrap(),
        serde_json::to_string(&k.ledger.entries()).unwrap(),
        events.lock().unwrap(),
        dumped
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reply_secret_from_a_is_secret_in_bs_records() {
    for wire in WIRES {
        let p = pair(wire, true, true).await;
        let set = op(
            &p.ka,
            &p.n("creds"),
            json!({"type": "set", "ref": REF, "fields": {"api_key": VALUE}}),
        )
        .await;
        assert_eq!(set.result["stored"], json!(true), "{}", set.result);
        let events: Arc<Mutex<Vec<Value>>> = Arc::default();
        let tap = Arc::clone(&events);
        let token = p.kb.add_state_subscriber(Arc::new(move |ev: &Value, _b| {
            tap.lock().unwrap().push(ev.clone());
        }));
        let out =
            p.kb.send(vec![Item::new(
                p.remote("creds"),
                json!({"type": "get", "ref": REF}),
            )
            .with_id("b-get-1")])
                .await
                .remove(0);
        // the caller gets it whole
        assert_eq!(
            out.result["fields"]["api_key"],
            json!(VALUE),
            "{wire:?}: {}",
            out.result
        );
        let kept = everything_kept(&p.kb, &events).await;
        p.kb.remove_state_subscriber(token);
        assert!(kept.contains("b-get-1"), "{wire:?}: not recorded: {kept}");
        assert!(kept.contains("[secret]"), "{wire:?}: {kept}");
        assert!(!kept.contains(VALUE), "{wire:?}: B kept the value: {kept}");
        p.stop().await;
    }
}

// ─── reconnects ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_link_fails_calls_fast_and_a_redial_resumes_the_watch() {
    for wire in WIRES {
        let p = pair(wire, true, true).await;
        let target = p.remote("ai");
        let w = AgentId::from("watcher");
        let mut rx = p.kb.ensure_inbox(&w);
        assert_eq!(p.kb.watch(&target, w.clone()).await, None, "{wire:?}");
        p.until_watched("ai", true).await;

        // a call in flight when the link drops fails at once, `bridge_down`
        let (kb, probe) = (Arc::clone(&p.kb), p.remote("probe"));
        let inflight =
            tokio::spawn(async move { op(&kb, probe.as_str(), json!({"type": "hold"})).await });
        tokio::time::timeout(Duration::from_secs(5), p.held.notified())
            .await
            .expect("A runs the call");
        p.cut().await;
        let failed = tokio::time::timeout(Duration::from_secs(3), inflight)
            .await
            .expect("the in-flight call settles, no hang")
            .expect("task");
        assert_eq!(
            failed.result["reason"],
            json!("transport_error"),
            "{wire:?}: {:?}",
            failed.result
        );
        assert!(
            failed.result["error"]
                .as_str()
                .unwrap()
                .starts_with("bridge_down:"),
            "{wire:?}: {:?}",
            failed.result
        );
        // while the link is down, what A's AI streams is not kept for B
        let local = op(&p.ka, &p.n("ai"), turn("unseen")).await;
        assert_eq!(local.status.as_str(), "completed", "{wire:?}");

        // A redials and says hello again: B's watch resumes without B asking
        p.dial().await;
        p.until_watched("ai", true).await;
        assert_eq!(drain(&mut rx), Vec::<Value>::new(), "{wire:?}: replayed");
        let out = p.call("ai", turn("again")).await;
        assert_eq!(
            out.status.as_str(),
            "completed",
            "{wire:?}: {:?}",
            out.result
        );
        assert_stream_before_done(&drain(&mut rx), wire);
        p.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_hello_for_the_guid_replaces_the_first_connection() {
    for wire in WIRES {
        let p = pair(wire, true, true).await;
        let mut newer = raw_ws(p.port, &p.b_web).await;
        newer
            .send(TMessage::Text(
                json!({"type": "hello", "guid": A, "leg": "newer"}).to_string(),
            ))
            .await
            .unwrap();
        // the older connection is closed: A's leg sees its link go
        let end = tokio::time::Instant::now() + Duration::from_secs(3);
        while p.reflect().await["connected"] == json!(true) {
            assert!(
                tokio::time::Instant::now() < end,
                "{wire:?}: the older connection stayed open"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(p.kb.peer_routes(A).len(), 1, "{wire:?}");
        // and B's calls to A's guid ride the newer one
        let (kb, probe) = (Arc::clone(&p.kb), p.remote("probe"));
        let call = tokio::spawn(async move {
            op(&kb, probe.as_str(), json!({"type": "echo", "text": "n"})).await
        });
        let frame = next_of(&mut newer, "call").await;
        assert_eq!(frame["target"], json!(p.n("probe")), "{wire:?}: {frame}");
        newer
            .send(TMessage::Text(
                json!({"type": "reply", "id": frame["id"], "data": {"from": "newer"}}).to_string(),
            ))
            .await
            .unwrap();
        let out = call.await.expect("task");
        assert_eq!(out.result, json!({"from": "newer"}), "{wire:?}");
        newer.close(None).await.unwrap();
        until("B forgot the newer leg", || p.kb.peer_routes(A).is_empty()).await;
        p.stop().await;
    }
}
