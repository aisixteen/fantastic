//! LIVE GENESIS — the real experiment: ONE bare completion call carrying the
//! genesis prompt, and the model AUTHORS its own agent (control harness
//! included, shape of its choosing) as a persisted script agent; we then
//! hand the authored mind a task that requires acting through the kernel
//! and observe the feedback behavior. Assertions are BEHAVIORAL — never
//! about the harness shape. The authored code is printed as a reviewable
//! artifact (which executor it picked, which shape it declared).
//!
//! Env-gated (nothing is guessed — set all three or the test skips):
//!   FANTASTIC_AI_MODEL       e.g. a NIM-hosted model id
//!   FANTASTIC_NIM_ENDPOINT   e.g. an OpenAI-compatible NIM base URL (…/v1)
//!   NVIDIA_API_KEY           the bearer key
//!
//! Run:  cargo test -p fantastic-host --test mind_genesis_live -- --ignored --nocapture

use fantastic_kernel::AgentId;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const GENESIS: &str = include_str!("genesis.txt");
const NIM_ID: &str = "nim_organ";

struct LiveEnv {
    model: String,
    endpoint: String,
    api_key: String,
}

fn live_env() -> Option<LiveEnv> {
    let need = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    match (
        need("FANTASTIC_AI_MODEL"),
        need("FANTASTIC_NIM_ENDPOINT"),
        need("NVIDIA_API_KEY"),
    ) {
        (Some(model), Some(endpoint), Some(api_key)) => Some(LiveEnv {
            model,
            endpoint,
            api_key,
        }),
        _ => {
            eprintln!(
                "SKIP: set FANTASTIC_AI_MODEL + FANTASTIC_NIM_ENDPOINT + NVIDIA_API_KEY \
                 to run the live genesis (nothing is guessed)"
            );
            None
        }
    }
}

/// Extract the ONE create_agent JSON object from a completion (models
/// sometimes fence or preface despite instructions — strip to the outermost
/// braces).
fn extract_envelope(completion: &str) -> Option<Value> {
    let start = completion.find('{')?;
    let tail = &completion[start..];
    // Parse the FIRST complete JSON value from `tail` and IGNORE anything after
    // it — models fence/preface, append prose, or emit a stray trailing `}`
    // (bracketing slip); a first-value parse tolerates all of that where a
    // `rfind('}')` slice does not. Retry with `\'` -> `'` too: models sometimes
    // JS-escape a single quote inside the JSON string (invalid JSON, intended
    // `'`). Both are safe leniencies — mirror the RAW tool-parser's.
    let parse_first = |s: &str| -> Option<Value> {
        serde_json::Deserializer::from_str(s)
            .into_iter::<Value>()
            .next()?
            .ok()
    };
    let v = parse_first(tail).or_else(|| parse_first(&tail.replace("\\'", "'")))?;
    (v.get("type").and_then(Value::as_str) == Some("create_agent")).then_some(v)
}

async fn genesis_run(force_executor: &str) {
    let Some(env) = live_env() else { return };

    // Kernel rooted at a tempdir with the full relevant bundle set.
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
                   "file_bridge_id":"ff_live", "model": env.model, "endpoint": env.endpoint}),
        )
        .await;
    assert!(r.get("error").is_none(), "create nim: {r:#?}");
    kernel
        .send(
            &AgentId::from(NIM_ID),
            json!({"type":"set_api_key","api_key": env.api_key}),
        )
        .await;

    // Build the live agent menu from a real reflect.
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
                        e.get("handler_module").and_then(Value::as_str).unwrap_or("(bare)")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let mut genesis = GENESIS
        .replace("{AGENT_MENU}", &menu)
        .replace("{NIM_ID}", NIM_ID);
    genesis.push_str(&format!(
        "\nFor this run you MUST use handler_module \"{force_executor}\".\n"
    ));

    // ── ONE bare completion call: the genesis ask. ──
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
    println!("\n════ GENESIS COMPLETION ({force_executor}) ════\n{completion}\n");

    let envelope = extract_envelope(completion)
        .unwrap_or_else(|| panic!("no create_agent envelope in the completion"));
    let code = envelope["code"].as_str().unwrap_or("");
    println!(
        "════ AUTHORED MIND ({} · first line: {:?}) ════\n{code}\n",
        envelope["handler_module"].as_str().unwrap_or("?"),
        code.lines().next().unwrap_or("")
    );

    // ── The mechanical relay: execute the ONE envelope. ──
    let created = kernel.send(&AgentId::from("kernel"), envelope.clone()).await;
    assert!(created.get("error").is_none(), "create mind: {created:#?}");
    let mind_id = envelope["id"].as_str().unwrap_or("mind").to_string();
    let st = kernel
        .send(&AgentId::from(mind_id.as_str()), json!({"type":"status"}))
        .await;
    assert_eq!(st["running"], true, "authored mind booted: {st:#?}");

    // Watch the NIM organ so further authored calls are observable.
    kernel
        .watch(&AgentId::from(NIM_ID), AgentId::from("nim_watch"))
        .await;
    let (wtx, mut wrx) = tokio::sync::mpsc::channel::<Value>(1024);
    kernel.inboxes.insert(AgentId::from("nim_watch"), wtx);

    // Listen to the mind's emits, then hand it a task that requires ACTING.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(1024);
    kernel.inboxes.insert(AgentId::from(mind_id.as_str()), tx);
    let ack = kernel
        .send(
            &AgentId::from(mind_id.as_str()),
            json!({"type":"send",
                   "text":"How many agents live in this kernel right now? Find out by asking the kernel, then answer with the number.",
                   "client_id":"probe"}),
        )
        .await;
    assert!(
        ack["queued"] == true || ack.get("error").is_none(),
        "mind accepted the turn: {ack:#?}"
    );

    // Behavioral collection: says + done from the mind, sends to the organ.
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
                if ev["type"] == "send" {
                    organ_calls += 1;
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }

    // ── Behavior, never shape. ──
    println!(
        "\n════ VERDICT ({force_executor}) ════\nfurther organ calls: {organ_calls} · says: {} · done: {done}",
        says.len()
    );
    assert!(
        organ_calls >= 1,
        "the authored mind must call the completion organ at least once for the task"
    );
    assert!(!says.is_empty(), "the authored mind must report via say");
    assert!(done, "the authored mind must finish the turn with done");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live model call — set FANTASTIC_AI_MODEL + FANTASTIC_NIM_ENDPOINT + NVIDIA_API_KEY"]
async fn genesis_authors_a_lua_mind() {
    genesis_run("lua_runtime.tools").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live model call — set FANTASTIC_AI_MODEL + FANTASTIC_NIM_ENDPOINT + NVIDIA_API_KEY"]
async fn genesis_authors_a_js_mind() {
    genesis_run("js_runtime.tools").await;
}
