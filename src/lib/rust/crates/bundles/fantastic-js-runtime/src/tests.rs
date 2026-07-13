//! js_runtime tests — mirror of the lua suite: echo round-trip, connector
//! send, self-send guard, stop/delete abort, reboot rehydration.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;
use std::time::Duration;
use tempfile::TempDir;

fn id_for(tmp: &TempDir, tag: &str) -> String {
    format!(
        "js_{}_{}",
        tag,
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

fn mk_kernel(tmp: &TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    // Disk mode so records persist through a wired store (reboot test).
    kernel.storage = fantastic_kernel::storage::StorageMode::Disk(tmp.path().join(".fantastic"));
    kernel.bundles.register(HANDLER_MODULE, JsRuntimeBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

async fn create_script(kernel: &Arc<Kernel>, id: &str, code: &str) {
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": id,
                "code": code,
            }),
        )
        .await;
}

fn agent_inbox(kernel: &Arc<Kernel>, id: &AgentId) -> tokio::sync::mpsc::Receiver<Value> {
    let (tx, rx) = tokio::sync::mpsc::channel(kernel.inbox_bound);
    kernel.inboxes.insert(id.clone(), tx);
    rx
}

async fn recv_until(
    rx: &mut tokio::sync::mpsc::Receiver<Value>,
    pred: impl Fn(&Value) -> bool,
    secs: u64,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Some(v)) if pred(&v) => return Some(v),
            Ok(Some(_)) => {}
            Ok(None) => return None,
            Err(_) => {}
        }
    }
    None
}

/// Like `recv_until`, but also returns EVERY event seen up to (and
/// including) the match — lets a test assert on the events that arrived
/// *before* the one it was waiting for (e.g. "no `done` leaked before the
/// question", mirroring the Swift `EventSink`).
async fn collect_until(
    rx: &mut tokio::sync::mpsc::Receiver<Value>,
    pred: impl Fn(&Value) -> bool,
    secs: u64,
) -> (Option<Value>, Vec<Value>) {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Some(v)) => {
                let hit = pred(&v);
                seen.push(v.clone());
                if hit {
                    return (Some(v), seen);
                }
            }
            Ok(None) => return (None, seen),
            Err(_) => {}
        }
    }
    (None, seen)
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("js_runtime"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_script_round_trips_inbox_to_emit() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    let id = id_for(&tmp, "echo");
    let code = r#"
        while (true) {
            const msg = await kernel.on_message();
            if (msg === null) break;
            await kernel.emit({ type: "echo", text: msg.text, client_id: msg.client_id });
        }
    "#;
    create_script(&kernel, &id, code).await;
    let agent = AgentId::from(id.as_str());
    let mut rx = agent_inbox(&kernel, &agent);
    let st = kernel.send(&agent, json!({"type": "status"})).await;
    assert_eq!(st["running"], true, "boot started the script: {st:#?}");

    let r = kernel
        .send(
            &agent,
            json!({"type": "send", "text": "hello mind", "client_id": "c1"}),
        )
        .await;
    assert_eq!(r["queued"], true, "inbox ack: {r:#?}");
    let echoed = recv_until(&mut rx, |v| v["type"] == "echo", 3)
        .await
        .expect("script echoed the payload");
    assert_eq!(echoed["text"], "hello mind");
    assert_eq!(echoed["client_id"], "c1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_kernel_resolves_the_connector() {
    // Every JS-trained model reaches for the Node idiom `const kernel =
    // require('kernel')` (observed on gemma4:12b AND gpt-oss:20b). The runtime
    // speaks it: `require('kernel')` returns the SAME connector as the global —
    // not a second source of truth — and any other name throws.
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    let id = id_for(&tmp, "req");
    // VERBATIM the shape a live gemma4:12b authored (and gpt-oss:20b too): the Node
    // `require` idiom, a `main()` wrapper, and `.catch(console.error)`. QuickJS ships
    // neither `require` nor `console`, so both were ReferenceErrors that killed the
    // script before its first line — the tool "existed" but never ran.
    // VERBATIM what a live gemma4:12b authored — including the UN-awaited `emit`s.
    let code = r#"
        const kernel = require('kernel');
        async function main() {
          while (true) {
            const msg = await kernel.on_message();
            if (msg.type === 'send') {
              kernel.emit({ type: 'echo', text: msg.text, client_id: msg.client_id });
              kernel.emit({ type: 'done', client_id: msg.client_id });
            }
          }
        }
        main().catch(console.error);
    "#;
    create_script(&kernel, &id, code).await;
    let agent = AgentId::from(id.as_str());
    let mut rx = agent_inbox(&kernel, &agent);
    let st = kernel.send(&agent, json!({"type": "status"})).await;
    assert_eq!(st["running"], true, "require() script booted: {st:#?}");

    kernel
        .send(
            &agent,
            json!({"type": "send", "text": "hello world", "client_id": "me"}),
        )
        .await;
    let echoed = recv_until(&mut rx, |v| v["type"] == "echo", 3)
        .await
        .expect("require('kernel') script echoed");
    assert_eq!(echoed["text"], "hello world");
    assert_eq!(echoed["client_id"], "me");

    // A DURABLE tool must serve many turns, not one. A detached entry point
    // (`main().catch(...)`) leaves the loop pending after the wrapper settles; with
    // nothing driving the runtime it served exactly ONE message and then died —
    // silently, while still reporting `running: true`. Prove it keeps answering.
    for n in 0..5 {
        let probe = format!("turn {n}");
        kernel
            .send(
                &agent,
                json!({"type": "send", "text": probe, "client_id": "me"}),
            )
            .await;
        let got = recv_until(&mut rx, |v| v["type"] == "echo" && v["text"] == probe, 5)
            .await
            .unwrap_or_else(|| panic!("detached loop must still answer on turn {n}"));
        assert_eq!(got["text"], probe);
    }
    let st = kernel.send(&agent, json!({"type": "status"})).await;
    assert_eq!(
        st["running"], true,
        "script still alive after many turns: {st:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn script_send_reaches_agents_and_returns_reply() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    let id = id_for(&tmp, "send");
    let code = r#"
        const reply = await kernel.send("kernel", { type: "list_agents" });
        const n = Array.isArray(reply.agents) ? reply.agents.length : 0;
        await kernel.emit({ type: "counted", agents: n, self_id: kernel.self_id });
    "#;
    create_script(&kernel, &id, code).await;
    let agent = AgentId::from(id.as_str());
    let mut rx = agent_inbox(&kernel, &agent);
    kernel.send(&agent, json!({"type": "start"})).await;
    let counted = recv_until(&mut rx, |v| v["type"] == "counted", 3)
        .await
        .expect("script sent + emitted");
    assert!(
        counted["agents"].as_i64().unwrap_or(0) >= 1,
        "list_agents reached the kernel: {counted:#?}"
    );
    assert_eq!(counted["self_id"], id.as_str());
}

/// The connector's guard refuses a chain that REVISITS this agent: a
/// payload arriving with our id already in `_call_stack` cannot be
/// forwarded onward — cycle refusal. (A fresh send to oneself is
/// legitimate MAIL: fresh chain, queued, consumed.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revisiting_chain_is_refused_by_guard() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    let id = id_for(&tmp, "cycle");
    let code = format!(
        r#"
        const reply = await kernel.send("kernel", {{ type: "list_agents", _call_stack: ["{id}"] }});
        await kernel.emit({{ type: "verdict", err: reply.error }});
    "#
    );
    create_script(&kernel, &id, &code).await;
    let agent = AgentId::from(id.as_str());
    let mut rx = agent_inbox(&kernel, &agent);
    kernel.send(&agent, json!({"type": "start"})).await;
    let verdict = recv_until(&mut rx, |v| v["type"] == "verdict", 3)
        .await
        .expect("script got the refusal");
    assert!(
        verdict["err"].as_str().is_some_and(|e| e.contains("cycle")),
        "revisiting chain must be refused: {verdict:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_aborts_and_delete_cleans_up() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    let id = id_for(&tmp, "stop");
    let code = "while (true) { await kernel.on_message(); }";
    create_script(&kernel, &id, code).await;
    let agent = AgentId::from(id.as_str());
    let st = kernel.send(&agent, json!({"type": "status"})).await;
    assert_eq!(st["running"], true);
    let r = kernel.send(&agent, json!({"type": "stop"})).await;
    assert_eq!(r["stopped"], true);
    let st2 = kernel.send(&agent, json!({"type": "status"})).await;
    assert_eq!(st2["running"], false);
    let dead = kernel
        .send(&agent, json!({"type": "send", "text": "x"}))
        .await;
    assert!(dead["error"].as_str().is_some(), "{dead:#?}");
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type": "delete_agent", "id": id}),
        )
        .await;
    assert!(!running(&agent));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_meta_survives_reboot_and_boot_revives() {
    let tmp = TempDir::new().unwrap();
    let id;
    {
        let kernel = mk_kernel(&tmp);
        // Native persistence writes through a STORE (a file_bridge child
        // rooted at the root's own dir) — wire one, like the gateway seed.
        kernel
            .send(
                &AgentId::from("core"),
                json!({
                    "type": "create_agent",
                    "handler_module": "file_bridge.tools",
                    "id": "store",
                    "root": tmp.path().join(".fantastic").to_string_lossy(),
                    "ingress_rule": "allow_all",
                }),
            )
            .await;
        id = id_for(&tmp, "reboot");
        create_script(
            &kernel,
            &id,
            "while (true) { const m = await kernel.on_message(); if (m === null) break; await kernel.emit({ type: 'alive' }); }",
        )
        .await;
        let agent = AgentId::from(id.as_str());
        let st = kernel.send(&agent, json!({"type": "status"})).await;
        assert_eq!(st["running"], true);
        // Give the store a beat to flush the record.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let rec = tmp
            .path()
            .join(".fantastic")
            .join("agents")
            .join(&id)
            .join("agent.json");
        assert!(rec.exists(), "record persisted: {rec:?}");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&rec).unwrap()).unwrap();
        assert!(v["code"].as_str().is_some_and(|c| c.contains("on_message")));
        kernel.send(&agent, json!({"type": "shutdown"})).await;
    }
    {
        let kernel = mk_kernel(&tmp);
        let rec = tmp
            .path()
            .join(".fantastic")
            .join("agents")
            .join(&id)
            .join("agent.json");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&rec).unwrap()).unwrap();
        let mut create = json!({
            "type": "create_agent",
            "handler_module": HANDLER_MODULE,
            "id": id,
        });
        create["code"] = v["code"].clone();
        kernel.send(&AgentId::from("core"), create).await;
        let agent = AgentId::from(id.as_str());
        let st = kernel.send(&agent, json!({"type": "status"})).await;
        assert_eq!(
            st["running"], true,
            "boot revived the persisted script: {st:#?}"
        );
    }
}

// ── PLUMBED MIND: plan/execute + ask-the-human ──────────────────────
//
// A JS-authored mind on `js_runtime` drives a backend through a
// tool-feedback cycle: it lays out a PLAN, executes a `CALL` tool step,
// then `ASK`s the human — emitting a `{type:"question"}` event and
// SUSPENDING (no `done`) until the matching `{type:"answer"}` arrives as
// the next turn, which it feeds back as the next backend turn. Nothing in
// the backend or kernel changes; the mind OWNS the ask (emergent
// human-in-the-loop). Rust mirror of the Python canonical
// (`tests/test_mind_plumbed.py::test_mind_plans_executes_and_asks_single`)
// and the Swift mirror
// (`JSRuntimeTests.swift::plumbedMindPlansExecutesAndAsksTheHuman`) —
// closing the gap that the rust js/lua runtimes had NO plumbed-mind test.

/// Scripted stub backend state, behind an `Arc<Mutex<_>>` so the test keeps
/// a handle to assert on `calls`/`texts` after a clone is registered in the
/// kernel (satisfies the `Bundle: Sync` bound). Rust twin of the Swift
/// `StubBackend` / Python `_FakeProvider`.
#[derive(Default)]
struct StubState {
    /// One response popped per `send` call (front to back).
    scripts: Vec<String>,
    /// How many `send` calls arrived — proves the full feedback cycle ran.
    calls: usize,
    /// The `text` of every `send` — proves a human answer became the NEXT
    /// backend turn.
    texts: Vec<String>,
}

/// An agent that answers `send` from a scripted list (one per call) and
/// records every sent `text` + a call count.
struct StubBackend {
    state: Arc<Mutex<StubState>>,
}

#[async_trait]
impl Bundle for StubBackend {
    fn name(&self) -> &str {
        "stub_backend"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": "Scripted stub backend.",
                "verbs": {"send": "send"},
            }),
            "send" => {
                let mut st = self.state.lock().expect("stub state poisoned");
                st.calls += 1;
                st.texts.push(
                    payload
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                );
                let resp = if st.scripts.is_empty() {
                    String::new()
                } else {
                    st.scripts.remove(0)
                };
                json!({
                    "response": resp.clone(),
                    "final": resp,
                    "client_id": payload.get("client_id").cloned().unwrap_or(Value::Null),
                })
            }
            other => json!({"error": format!("stub: unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

/// `mk_kernel`, plus a caller-supplied extra bundle (registered BEFORE the
/// kernel is sealed into an `Arc`, the only point `bundles` is mutable).
fn mk_kernel_with<B: Bundle + 'static>(tmp: &TempDir, handler: &str, extra: B) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.storage = fantastic_kernel::storage::StorageMode::Disk(tmp.path().join(".fantastic"));
    kernel.bundles.register(HANDLER_MODULE, JsRuntimeBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    kernel.bundles.register(handler, extra);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

/// The authored mind — VERBATIM the contract from the Swift mirror. It
/// processes only `send` turns; on an `ASK` it emits a `question` and
/// suspends on `awaitAnswer`, resuming when the matching `answer` arrives.
const MIND_JS: &str = r#"
const SYSTEM = "plan/execute: PLAN: a|b, CALL <t> <v>, ASK <json>, FINAL: <a>";
let n = 0;
async function awaitAnswer(rid) {
    while (true) {
        const a = await kernel.on_message();
        if (a === null) return null;
        if (a.type === 'answer' && a.request_id === rid) return a;
    }
}
while (true) {
    const m = await kernel.on_message();
    if (m === null) break;
    if (m.type !== 'send') continue;
    let text = m.text;
    const cid = m.client_id;
    for (let i = 0; i < 16; i++) {
        const reply = await kernel.send('the_ai', { type: 'send', text: text, system_prompt: SYSTEM, client_id: cid });
        const resp = (((reply && (reply.response || reply.error)) || '')).trim();
        const body = resp.replace(/^STEP \d+:\s*/, '');
        if (body.indexOf('PLAN:') === 0) { text = 'Execute the plan step by step.'; continue; }
        const mm = /^CALL (\S+) (\S+)/.exec(body);
        if (mm) { const r = await kernel.send(mm[1], { type: mm[2] }); text = 'RESULT: ' + JSON.stringify(r); continue; }
        if (body.indexOf('ASK ') === 0) {
            const spec = JSON.parse(body.slice(4)); n += 1; const rid = 'q_' + n;
            const q = { type: 'question', request_id: rid, kind: spec.kind || 'text', question: spec.question || '', source: kernel.self_id, client_id: cid };
            if (spec.options) q.options = spec.options;
            await kernel.emit(q);
            const ans = await awaitAnswer(rid);
            if (ans && ans.action === 'accept') { text = 'ANSWER ' + rid + ': ' + JSON.stringify(ans.value); continue; }
            await kernel.emit({ type: 'say', text: '(aborted)', client_id: cid });
            await kernel.emit({ type: 'done', client_id: cid }); break;
        }
        const fin = body.indexOf('FINAL:') === 0 ? body.slice(6).trim() : body;
        await kernel.emit({ type: 'say', text: fin, client_id: cid });
        await kernel.emit({ type: 'done', client_id: cid }); break;
    }
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plumbed_mind_plans_executes_and_asks_the_human() {
    let tmp = TempDir::new().unwrap();
    // Scripted backend turns: plan → call → ask (single-choice) → final.
    let state = Arc::new(Mutex::new(StubState {
        scripts: vec![
            "PLAN: list, then confirm".into(),
            "STEP 1: CALL core list_agents".into(),
            "STEP 2: ASK {\"kind\":\"single\",\"question\":\"Proceed?\",\
             \"options\":[{\"value\":\"yes\"},{\"value\":\"no\"}]}"
                .into(),
            "FINAL: proceeded with yes".into(),
        ],
        ..Default::default()
    }));
    let kernel = mk_kernel_with(
        &tmp,
        "stub_backend.tools",
        StubBackend {
            state: Arc::clone(&state),
        },
    );
    // Register the stub backend under id `the_ai` BEFORE the mind exists.
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": "stub_backend.tools",
                "id": "the_ai",
            }),
        )
        .await;

    create_script(&kernel, "the_mind", MIND_JS).await;
    let mind = AgentId::from("the_mind");
    let mut rx = agent_inbox(&kernel, &mind);
    let st = kernel.send(&mind, json!({"type": "status"})).await;
    assert_eq!(st["running"], true, "the mind booted: {st:#?}");

    // 1. Open the turn — the unknown `send` verb is queued to the script.
    let ack = kernel
        .send(
            &mind,
            json!({"type": "send", "text": "set it up", "client_id": "probe"}),
        )
        .await;
    assert_eq!(ack["queued"], true, "inbox ack: {ack:#?}");

    // 2. The mind plans, executes the CALL, then ASKs — a `question` event
    //    surfaces and the turn SUSPENDS (no `done` leaks before it).
    let (qev, before) = collect_until(&mut rx, |v| v["type"] == "question", 8).await;
    let qev = qev.unwrap_or_else(|| panic!("no question emitted; saw {before:#?}"));
    assert_eq!(qev["kind"], "single", "single-choice question: {qev:#?}");
    assert!(
        qev.get("options").is_some(),
        "question carries options: {qev:#?}"
    );
    assert!(
        qev.get("source").is_some(),
        "question names its source: {qev:#?}"
    );
    assert_eq!(qev["client_id"], "probe");
    assert!(
        before.iter().all(|e| e["type"] != "done"),
        "no done before the question — the turn genuinely suspended: {before:#?}"
    );
    let rid = qev["request_id"]
        .as_str()
        .expect("question request_id")
        .to_string();

    // 3. Answer it → the chosen value becomes the next backend turn.
    let a = kernel
        .send(
            &mind,
            json!({"type": "answer", "request_id": rid, "action": "accept", "value": "yes"}),
        )
        .await;
    assert_eq!(a["queued"], true, "answer ack: {a:#?}");

    // 4. Only NOW does the turn seal — a `done`, preceded by the final `say`.
    let (done_ev, rest) = collect_until(&mut rx, |v| v["type"] == "done", 8).await;
    assert!(done_ev.is_some(), "turn never sealed; saw {rest:#?}");
    let say = rest
        .iter()
        .find(|e| e["type"] == "say")
        .unwrap_or_else(|| panic!("no say before done; saw {rest:#?}"));
    assert!(
        say["text"].as_str().unwrap_or("").contains("yes"),
        "final say surfaced the accepted choice: {say:#?}"
    );

    // 5. The full plan+execute+ask cycle really ran: 4 backend passes, and
    //    the human answer became one of the backend turns.
    let st = state.lock().expect("stub state poisoned");
    assert_eq!(
        st.calls, 4,
        "plan + call + ask + post-answer final = 4 passes"
    );
    assert!(
        st.texts.iter().any(|t| t.contains("yes")),
        "the human answer became a backend turn: {:#?}",
        st.texts
    );
}
