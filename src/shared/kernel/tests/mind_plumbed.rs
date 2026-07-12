//! The PLUMBED MIND — deterministic proof of the whole emergent-harness
//! chain with no live model: a hand-written Lua "mind" script receives a
//! user turn, teaches its tool convention via `system_prompt`, calls the
//! context-managed NIM organ (wiremock SSE — the organ HOLDS the conversation
//! and would auto-compact), interprets the completion by ITS OWN convention (a
//! line protocol it chose — nothing is baked anywhere), dispatches the action,
//! feeds the result back as the next turn, and reports via emits.
//!
//! user turn → mind (lua) → send{text, system_prompt} → "CALL kernel list_agents"
//!          → mind dispatches → feeds RESULT back as the next turn → send
//!          → "FINAL: …" → mind emits say + done.

use fantastic_kernel::AgentId;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The hand-written mind. Its action convention is a LINE protocol it
/// defines itself: `CALL <target> <verb>` → dispatch; `FINAL: <text>` →
/// answer. Any convention would do — that is the point.
const MIND_LUA: &str = r#"
-- shape: feedback loop (hand-written fixture)
local SYSTEM = "You drive a kernel. To act, reply EXACTLY `CALL <target> <verb>`. "
    .. "When you know the answer, reply EXACTLY `FINAL: <answer>`."
while true do
    local msg = kernel.on_message()
    if msg == nil then return end
    -- The organ HOLDS the conversation (keyed by client_id); the mind just sends
    -- text turns + its system_prompt and feeds tool results back as the next turn.
    local text = msg.text
    for _ = 1, 4 do
        local reply = kernel.send("nim_organ", {
            type = "send", text = text, system_prompt = SYSTEM, client_id = msg.client_id,
        })
        if reply.error ~= nil then
            kernel.emit({ type = "say", text = "ERR: " .. reply.error, client_id = msg.client_id })
            kernel.emit({ type = "done", client_id = msg.client_id })
            break
        end
        local resp = reply.response or ""
        local target, verb = string.match(resp, "^CALL (%S+) (%S+)")
        if target ~= nil then
            local r = kernel.send(target, { type = verb })
            text = "RESULT: " .. kernel.json_encode(r)
        else
            local answer = string.match(resp, "^FINAL: (.*)$") or resp
            kernel.emit({ type = "say", text = answer, client_id = msg.client_id })
            kernel.emit({ type = "done", client_id = msg.client_id })
            break
        end
    end
end
"#;

fn sse(chunks: &[&str]) -> String {
    let mut out = String::new();
    for c in chunks {
        let line = json!({"choices":[{"delta":{"content": c}}]}).to_string();
        out.push_str(&format!("data: {line}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hand_written_lua_mind_drives_bare_nim_through_a_feedback_cycle() {
    let server = MockServer::start().await;
    // Pass 1: the model "decides" to act.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(&["CALL kernel list_agents"])),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    // Pass 2: with the RESULT fed back, it answers.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(&["FINAL: the kernel is alive and populated"])),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Hand-rolled kernel rooted at a tempdir (same shape as the backend +
    // runtime unit tests): file bridge for the api_key sidecar, the bare NIM
    // organ, and the lua executor.
    let tmp = tempfile::tempdir().unwrap();
    let mut kernel = fantastic_kernel::Kernel::new();
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    kernel.bundles.register(
        "nvidia_nim_backend.tools",
        fantastic_nvidia_nim_backend::NvidiaNimBundle,
    );
    kernel.bundles.register(
        fantastic_lua_runtime::HANDLER_MODULE,
        fantastic_lua_runtime::LuaRuntimeBundle,
    );
    let kernel = Arc::new(kernel);
    let root = fantastic_kernel::Agent::new(
        AgentId::from("core"),
        None,
        None,
        serde_json::Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _root_rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    // A file agent for the NIM api_key sidecar, then the BARE organ itself.
    kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"file_bridge.tools","id":"ff_mind",
                   "root": tmp.path().join(".fantastic").to_string_lossy(), "ingress_rule":"allow_all"}),
        )
        .await;
    let r = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"nvidia_nim_backend.tools","id":"nim_organ",
                   "file_bridge_id":"ff_mind", "model":"test-model",
                   "endpoint": format!("{}/v1", server.uri())}),
        )
        .await;
    assert!(r.get("error").is_none(), "create nim: {r:#?}");
    kernel
        .send(
            &AgentId::from("nim_organ"),
            json!({"type":"set_api_key","api_key":"nvapi-test"}),
        )
        .await;

    // The mind — an authored agent on the lua executor.
    let r = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"lua_runtime.tools","id":"mind",
                   "code": MIND_LUA}),
        )
        .await;
    assert!(r.get("error").is_none(), "create mind: {r:#?}");
    let st = kernel
        .send(&AgentId::from("mind"), json!({"type":"status"}))
        .await;
    assert_eq!(st["running"], true, "mind booted: {st:#?}");

    // Listen to the mind's emits, then hand it a user turn.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(256);
    kernel.inboxes.insert(AgentId::from("mind"), tx);
    let ack = kernel
        .send(
            &AgentId::from("mind"),
            json!({"type":"send","text":"what lives in this kernel?","client_id":"probe"}),
        )
        .await;
    assert_eq!(ack["queued"], true, "{ack:#?}");

    // Collect until done.
    let mut say: Option<Value> = None;
    let mut done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !done {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(ev)) => match ev["type"].as_str() {
                Some("say") => say = Some(ev),
                Some("done") => done = true,
                _ => {}
            },
            Ok(None) => break,
            Err(_) => {}
        }
    }
    assert!(done, "mind must finish the turn with a done event");
    let say = say.expect("mind must emit a say");
    assert_eq!(say["client_id"], "probe");
    assert!(
        say["text"]
            .as_str()
            .is_some_and(|t| t.contains("alive and populated")),
        "final answer surfaced: {say:#?}"
    );

    // The feedback actually flowed: the SECOND provider request carried the
    // mind's RESULT turn with real list_agents output.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2, "two bare passes");
    let second: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let body = second["messages"].to_string();
    assert!(
        body.contains("RESULT:") && body.contains("agents"),
        "tool result fed back into the second pass: {body}"
    );
}
