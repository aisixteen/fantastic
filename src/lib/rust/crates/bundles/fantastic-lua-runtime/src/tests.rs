//! lua_runtime tests: echo round-trip, connector send, call-stack guard,
//! stop/delete abort, reboot rehydration. No LLM anywhere — the executor
//! is general-purpose.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;
use std::time::Duration;
use tempfile::TempDir;

fn id_for(tmp: &TempDir, tag: &str) -> String {
    format!(
        "lua_{}_{}",
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
    kernel.bundles.register(HANDLER_MODULE, LuaRuntimeBundle);
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

/// Rebind the agent's inbox so the test can drain its emits.
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
    assert!(README.contains("lua_runtime"));
}

/// Echo script: every inbox payload comes back as an emit — the full
/// addressable-inbox → on_message → emit contract in one test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_script_round_trips_inbox_to_emit() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    let id = id_for(&tmp, "echo");
    let code = r#"
        while true do
            local msg = kernel.on_message()
            if msg == nil then return end
            kernel.emit({ type = "echo", text = msg.text, client_id = msg.client_id })
        end
    "#;
    create_script(&kernel, &id, code).await;
    let agent = AgentId::from(id.as_str());
    let mut rx = agent_inbox(&kernel, &agent);
    // create_agent auto-dispatches boot → the persisted code is running.
    let st = kernel.send(&agent, json!({"type": "status"})).await;
    assert_eq!(st["running"], true, "boot started the script: {st:#?}");

    // An unknown verb is the script's mail.
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

/// Sandbox-by-omission is REAL: the dangerous stdlib (`os`, `io`, `require`) is
/// absent, but pure computation (`string`, `table`, `math`) and the kernel
/// connector work. Guards the "only capability is the kernel connector" contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sandbox_omits_os_io_but_keeps_pure_libs() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    let id = id_for(&tmp, "sbx");
    let code = r#"
        kernel.emit({
            type = "probe",
            has_os = (os ~= nil),
            has_io = (io ~= nil),
            has_require = (require ~= nil),
            has_package = (package ~= nil),
            math_ok = (math.floor(3.7) == 3),
            string_ok = (string.upper("a") == "A"),
            table_ok = (#({1,2,3}) == 3),
        })
    "#;
    create_script(&kernel, &id, code).await;
    let agent = AgentId::from(id.as_str());
    let mut rx = agent_inbox(&kernel, &agent);
    kernel.send(&agent, json!({"type": "start"})).await;
    let p = recv_until(&mut rx, |v| v["type"] == "probe", 3)
        .await
        .expect("probe emitted");
    assert_eq!(p["has_os"], false, "os must be sandboxed out");
    assert_eq!(p["has_io"], false, "io must be sandboxed out");
    assert_eq!(
        p["has_require"], false,
        "require/package must be sandboxed out"
    );
    assert_eq!(p["has_package"], false, "package must be sandboxed out");
    assert_eq!(p["math_ok"], true, "math must remain");
    assert_eq!(p["string_ok"], true, "string must remain");
    assert_eq!(p["table_ok"], true, "table must remain");
}

/// A script `send`s another agent by id and reads the actual reply —
/// the connector suspends/resumes across a real kernel dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn script_send_reaches_agents_and_returns_reply() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    let id = id_for(&tmp, "send");
    // Ask the kernel for the agent list, emit how it went.
    let code = r#"
        local reply = kernel.send("kernel", { type = "list_agents" })
        local n = 0
        if reply.agents ~= nil then n = #reply.agents end
        kernel.emit({ type = "counted", agents = n, self_id = kernel.self_id })
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
/// payload arriving with our id already in `_call_stack` (someone up the
/// chain was us) cannot be forwarded onward — cycle refusal. (A fresh
/// send to oneself is legitimate MAIL: fresh chain, queued, consumed.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revisiting_chain_is_refused_by_guard() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp);
    let id = id_for(&tmp, "cycle");
    // The script forwards a payload that ALREADY carries itself in the
    // chain (as if A → us → … → back through us).
    let code = format!(
        r#"
        local reply = kernel.send("kernel", {{ type = "list_agents", _call_stack = {{ "{id}" }} }})
        kernel.emit({{ type = "verdict", err = reply.error }})
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
    let code = "while true do kernel.on_message() end";
    create_script(&kernel, &id, code).await;
    let agent = AgentId::from(id.as_str());
    let st = kernel.send(&agent, json!({"type": "status"})).await;
    assert_eq!(st["running"], true);
    let r = kernel.send(&agent, json!({"type": "stop"})).await;
    assert_eq!(r["stopped"], true);
    let st2 = kernel.send(&agent, json!({"type": "status"})).await;
    assert_eq!(st2["running"], false);
    // Mail with no running script is a loud error, not a silent queue.
    let dead = kernel
        .send(&agent, json!({"type": "send", "text": "x"}))
        .await;
    assert!(dead["error"].as_str().is_some(), "{dead:#?}");
    // Delete cleans the job slot.
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type": "delete_agent", "id": id}),
        )
        .await;
    assert!(!running(&agent));
}

/// The durable-authored-agent contract: `code` on the record survives a
/// re-bootstrap of the same workdir, and `boot` revives the script.
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
            "while true do local m = kernel.on_message(); if m == nil then return end; kernel.emit({ type = 'alive' }) end",
        )
        .await;
        let agent = AgentId::from(id.as_str());
        let st = kernel.send(&agent, json!({"type": "status"})).await;
        assert_eq!(st["running"], true);
        // The record (incl. `code`) is on disk (give the store a beat).
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
        // Simulate shutdown.
        kernel.send(&agent, json!({"type": "shutdown"})).await;
    }
    // "Reboot": fresh kernel over the same workdir; re-create the agent from
    // its persisted record fields (the bootstrap loader path does exactly
    // this: record meta → create → boot).
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
