//! LIVE — a NON-LOOP emergent harness. The model authors a `js_runtime` mind
//! whose control flow is SCATTER-GATHER + JUDGE (not iteration): for one turn it
//! generates THREE independent candidate answers (three organ calls, different
//! framings, no feedback between them), then ONE judge call selects the best,
//! then emits say + done. Falsifiable signature: for a SINGLE user turn the
//! organ is called >= 3 times — a shape a loop does not produce.
//!
//! Also a probe of the prompt-vs-reflect question: the js API here is stated
//! tersely (the same words the js_runtime `reflect` returns), and the ONLY
//! prose beyond the API is the SHAPE intent + the two usage patterns that live
//! runs showed reflect alone does not convey (on_message is a pull; JSON string
//! encoding). See the run's VERDICT + the assessment in chat.
//!
//! Env-gated (nothing guessed): FANTASTIC_AI_MODEL + FANTASTIC_NIM_ENDPOINT +
//! NVIDIA_API_KEY. Run:
//!   cargo test -p fantastic-host --test mind_judge_live -- --ignored --nocapture

use fantastic_kernel::AgentId;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const NIM_ID: &str = "nim_organ";

const JUDGE_GENESIS: &str = r#"You are inside a running agent kernel. ONE primitive: send(target_id, payload) —
a JSON payload whose "type" names the verb. Agents reply JSON and can emit events;
an event tagged with a client_id reaches that caller.

AGENTS PRESENT:
{AGENT_MENU}

THE COMPLETION ORGAN `{NIM_ID}` is a context-managed LLM completion organ. Call:
  send("{NIM_ID}", {"type":"send","text":"<turn>","system_prompt":"<system>","client_id":"<id>"})
It returns {"response":"<text>"}. It does ONE pass — no tool loop.

EXECUTOR "js_runtime.tools" — JavaScript, top-level await. Script API:
  await kernel.send(target, payload) -> reply object
  await kernel.emit(payload)
  await kernel.on_message() -> next inbox payload | null
  kernel.self_id
on_message is a PULL, not a callback: call it (no arguments) in a loop —
  while (true) { const m = await kernel.on_message(); if (m === null) break; ... }
never pass it a function.

YOUR TASK. Author a js_runtime agent that answers each user turn with a
SCATTER-GATHER + JUDGE control flow — NOT an iterative refine loop:
- for a turn, generate THREE INDEPENDENT candidate answers by calling the organ
  THREE times with three DIFFERENT framings in the system_prompt (independent —
  do NOT feed one candidate's output into the next);
- then make ONE more organ call, giving it all three candidates, that picks the
  single best one (the JUDGE);
- emit the winner as {"type":"say","text":<winner>,"client_id":...} then
  {"type":"done","client_id":...}. Every turn MUST end with BOTH say and done.
Begin the code with a one-line comment naming the shape.

ENCODING. The "code" value is ONE JSON string: separate statements with \n; NEVER
put a line break inside a string literal (keep each string's content on one line,
use a space not a newline even inside a system_prompt).

REPLY with ONLY one JSON object (no prose, no code fences):
{"type":"create_agent","handler_module":"js_runtime.tools","id":"judge","code":"<program as one JSON string>"}
"#;

fn env3() -> Option<(String, String, String)> {
    let need = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    match (
        need("FANTASTIC_AI_MODEL"),
        need("FANTASTIC_NIM_ENDPOINT"),
        need("NVIDIA_API_KEY"),
    ) {
        (Some(m), Some(e), Some(k)) => Some((m, e, k)),
        _ => {
            eprintln!("SKIP: set FANTASTIC_AI_MODEL + FANTASTIC_NIM_ENDPOINT + NVIDIA_API_KEY");
            None
        }
    }
}

/// First complete JSON value from the first `{`, tolerating fences/trailing junk
/// + the `\'` slip (mirrors mind_genesis_live's extractor).
fn extract_envelope(completion: &str) -> Option<Value> {
    let start = completion.find('{')?;
    let tail = &completion[start..];
    let parse_first = |s: &str| -> Option<Value> {
        serde_json::Deserializer::from_str(s)
            .into_iter::<Value>()
            .next()?
            .ok()
    };
    let v = parse_first(tail).or_else(|| parse_first(&tail.replace("\\'", "'")))?;
    (v.get("type").and_then(Value::as_str) == Some("create_agent")).then_some(v)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live model call — set FANTASTIC_AI_MODEL + FANTASTIC_NIM_ENDPOINT + NVIDIA_API_KEY"]
async fn model_authors_a_scatter_gather_judge_mind() {
    let Some((model, endpoint, api_key)) = env3() else {
        return;
    };

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
        fantastic_js_runtime::HANDLER_MODULE,
        fantastic_js_runtime::JsRuntimeBundle,
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

    kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"file_bridge.tools","id":"ff_live",
                   "root": tmp.path().join(".fantastic").to_string_lossy(), "ingress_rule":"allow_all"}),
        )
        .await;
    let r = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"nvidia_nim_backend.tools","id":NIM_ID,
                   "file_bridge_id":"ff_live", "model": model, "endpoint": endpoint}),
        )
        .await;
    assert!(r.get("error").is_none(), "create nim: {r:#?}");
    kernel
        .send(
            &AgentId::from(NIM_ID),
            json!({"type":"set_api_key","api_key": api_key}),
        )
        .await;

    let listed = kernel
        .send(&AgentId::from("kernel"), json!({"type":"list_agents"}))
        .await;
    let menu = listed["agents"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    format!(
                        "- {} — {}",
                        e.get("id").and_then(Value::as_str).unwrap_or("?"),
                        e.get("handler_module")
                            .and_then(Value::as_str)
                            .unwrap_or("(bare)")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let genesis = JUDGE_GENESIS
        .replace("{AGENT_MENU}", &menu)
        .replace("{NIM_ID}", NIM_ID);

    // ── ONE bare completion: the genesis ask. ──
    let reply = kernel
        .send(
            &AgentId::from(NIM_ID),
            json!({"type":"send","text": genesis, "client_id":"genesis"}),
        )
        .await;
    let completion = reply["response"].as_str().unwrap_or("");
    assert!(
        !completion.is_empty(),
        "genesis completion empty: {reply:#?}"
    );
    println!("\n════ GENESIS COMPLETION ════\n{completion}\n");

    let envelope =
        extract_envelope(completion).unwrap_or_else(|| panic!("no create_agent envelope"));
    let code = envelope["code"].as_str().unwrap_or("");
    println!(
        "════ AUTHORED MIND (shape line: {:?}) ════\n{code}\n",
        code.lines().next().unwrap_or("")
    );

    let created = kernel
        .send(&AgentId::from("kernel"), envelope.clone())
        .await;
    assert!(created.get("error").is_none(), "create mind: {created:#?}");
    let mind_id = envelope["id"].as_str().unwrap_or("judge").to_string();
    let st = kernel
        .send(&AgentId::from(mind_id.as_str()), json!({"type":"status"}))
        .await;
    assert_eq!(st["running"], true, "authored mind booted: {st:#?}");

    // Watch the organ (count calls) + the mind (say/done).
    kernel
        .watch(&AgentId::from(NIM_ID), AgentId::from("nim_watch"))
        .await;
    let (wtx, mut wrx) = tokio::sync::mpsc::channel::<Value>(1024);
    kernel.inboxes.insert(AgentId::from("nim_watch"), wtx);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(1024);
    kernel.inboxes.insert(AgentId::from(mind_id.as_str()), tx);

    let ack = kernel
        .send(
            &AgentId::from(mind_id.as_str()),
            json!({"type":"send",
                   "text":"Give the single best one-sentence definition of what an \"agent\" is in this kernel.",
                   "client_id":"probe"}),
        )
        .await;
    assert!(
        ack["queued"] == true || ack.get("error").is_none(),
        "mind accepted the turn: {ack:#?}"
    );

    let mut says: Vec<Value> = Vec::new();
    let mut done = false;
    let mut organ_calls = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    while tokio::time::Instant::now() < deadline && !done {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(ev) => {
                    println!("mind → {ev}");
                    match ev["type"].as_str() {
                        Some("say") => says.push(ev),
                        Some("done") => done = true,
                        _ => {}
                    }
                }
                None => break,
            },
            ev = wrx.recv() => if let Some(ev) = ev {
                if ev["type"] == "send" { organ_calls += 1; }
            },
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }

    println!(
        "\n════ VERDICT ════\norgan calls (one turn): {organ_calls} · says: {} · done: {done}",
        says.len()
    );
    // Scatter-gather+judge for ONE turn calls the organ multiple times with
    // INDEPENDENT candidates — a signature a single-pass or iterate-once loop
    // does not produce. >= 3 proves the fan-out shape emerged and ran.
    assert!(
        organ_calls >= 3,
        "a scatter-gather+judge makes several INDEPENDENT organ calls per turn; got {organ_calls}"
    );
    assert!(!says.is_empty(), "the mind must report a winner via say");
    assert!(done, "the mind must finish the turn with done");
}
