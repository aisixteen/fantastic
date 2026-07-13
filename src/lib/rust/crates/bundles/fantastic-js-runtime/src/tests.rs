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
