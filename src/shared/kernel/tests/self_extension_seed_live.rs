//! LIVE — validate the minimal `self_extension` seed with REFLECT-FIRST discovery.
//!
//! The honest test of "reflect by itself is enough": the model gets a LEAN system
//! prompt (the kernel model + the send primitive + "reflect to discover" + an
//! action convention) and NOTHING about executors, the script API, loops, or
//! escaping. It must REFLECT `kernel` to find `self_extension`, reflect the
//! executor / read the keystone readme to learn how, then AUTHOR a durable tool
//! and USE it — all discovered, not spoon-fed. A thin fixed driver relays the
//! model's `{act}` calls into the real kernel and feeds replies back.
//!
//! Rung 2 (author + use): the model must create a durable `js_runtime` tool that
//! echoes text as a `say`, then send it a turn — and the driver observes the tool
//! actually emit the echo. Falsifiable: the authored agent exists + running AND a
//! `say` carrying the probe text is emitted on it.
//!
//! Env-gated (nothing guessed): FANTASTIC_AI_MODEL (e.g. `glm-5:cloud`) — the
//! organ is a local ollama pointed at Ollama Cloud; endpoint defaults to
//! http://localhost:11434 (override FANTASTIC_OLLAMA_ENDPOINT). Run:
//!   cargo test -p fantastic-host --test self_extension_seed_live -- --ignored --nocapture

use fantastic_kernel::AgentId;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const ORGAN: &str = "organ";
const PROBE_TEXT: &str = "hello world";

const LEAN_SYSTEM: &str = r#"You are an agent inside a LIVE agent kernel. Everything here is an agent addressed by a string id. There is ONE primitive: send(target_id, payload) — a JSON payload whose "type" field names the verb; the target replies with JSON.

You act by replying with EXACTLY ONE JSON object and NOTHING else, in one of these two forms:
  {"act": {"target": "<id>", "payload": { ...a verb payload... }}}
  {"final": "<short message>"}
After an {"act"} I send it for you and reply with the result as your next message.

To LEARN anything, reflect it: {"act":{"target":"<id>","payload":{"type":"reflect"}}}. Reflecting "kernel" returns the system overview, your OWN capabilities, the agent tree, and a MAP of keyed docs (`readmes`) you can fetch one at a time. DISCOVER what you need by reflecting BEFORE you act — do not assume verb or API shapes.

You are ALONE and AUTONOMOUS — no human will reply between your turns. Never stop to ask, and never announce that you are "ready to proceed" or "understand now": that is NOT done. Reflecting and planning are not completing. Every reply must be a concrete {"act"} that moves the task forward. Only reply {"final":...} once the thing you were asked to build actually EXISTS and you have exercised it and seen it work. If you catch yourself about to declare readiness, instead emit the next {"act"} (a create_agent, or a send)."#;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

/// First complete JSON object from `s` (models fence/preface); tolerant of a
/// trailing brace and `\'` like the genesis extractor.
fn first_json(s: &str) -> Option<Value> {
    let start = s.find('{')?;
    let tail = &s[start..];
    let parse = |t: &str| -> Option<Value> {
        serde_json::Deserializer::from_str(t)
            .into_iter::<Value>()
            .next()?
            .ok()
    };
    parse(tail).or_else(|| parse(&tail.replace("\\'", "'")))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live model call — set FANTASTIC_AI_MODEL (local ollama → Ollama Cloud)"]
async fn seed_reflect_first_author_and_use() {
    let Some(model) = env("FANTASTIC_AI_MODEL") else {
        eprintln!("SKIP: set FANTASTIC_AI_MODEL (e.g. glm-5:cloud)");
        return;
    };
    let endpoint =
        env("FANTASTIC_OLLAMA_ENDPOINT").unwrap_or_else(|| "http://localhost:11434".into());

    // ── kernel: file_bridge store + ollama organ + js_runtime executor ──
    let tmp = tempfile::tempdir().unwrap();
    let mut kernel = fantastic_kernel::Kernel::new();
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    kernel.bundles.register(
        "ollama_backend.tools",
        fantastic_ollama_backend::OllamaBackendBundle,
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
    // Seed the root manual (SHORT orientation + the keyed `docs/` tree) to disk so
    // the model can `reflect readme=index` / `readme=<key>` and the root reflect's
    // `readmes` index is populated — the harness builds the kernel directly (no CLI
    // bootstrap), so nothing else seeds it.
    let _ = fantastic_core::seed_root_readme(tmp.path());

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
            json!({"type":"create_agent","handler_module":"ollama_backend.tools","id":ORGAN,
                   "file_bridge_id":"ff_live", "model": model, "endpoint": endpoint}),
        )
        .await;
    assert!(r.get("error").is_none(), "create organ: {r:#?}");

    // A watcher inbox we can point at the authored tool once it exists.
    let (wtx, mut wrx) = tokio::sync::mpsc::channel::<Value>(1024);
    kernel.inboxes.insert(AgentId::from("seed_watch"), wtx);

    let task = format!(
        "TASK: Author a DURABLE tool — a new agent whose behaviour you write — that, whenever it \
         is sent a message with type 'send' and a `text` field, replies by emitting a 'say' event \
         echoing that text back (tagged with the sender's client_id), then a 'done'. Name it \
         'greeter'. You must discover HOW to author such a tool (which executor, its script API, \
         its receive/emit pattern, and how to create/address it) by REFLECTING first. After it \
         exists, send it {{\"type\":\"send\",\"text\":\"{PROBE_TEXT}\",\"client_id\":\"me\"}} so it \
         echoes. Reply {{\"final\":...}} once you have sent that.",
    );

    // ── reflect-first relay loop ──
    let mut turn = task;
    let mut reflected_kernel = false;
    let mut saw_self_extension = false;
    let mut created_greeter = false;
    let mut watching = false;
    let mut acts = 0usize;
    let budget = 16usize;

    'outer: for step in 0..budget {
        let reply = kernel
            .send(
                &AgentId::from(ORGAN),
                json!({"type":"send","text": turn, "system_prompt": LEAN_SYSTEM, "client_id":"explorer"}),
            )
            .await;
        let resp = reply["response"].as_str().unwrap_or("");
        if let Some(err) = reply.get("error").and_then(Value::as_str) {
            panic!("organ error at step {step}: {err}");
        }
        println!("\n─── step {step} · model ───\n{resp}\n");

        let Some(action) = first_json(resp) else {
            turn = "Reply with ONE JSON object: {\"act\":{...}} or {\"final\":...}.".into();
            continue;
        };
        if action.get("final").is_some() {
            println!("model → final: {}", action["final"]);
            break;
        }
        let Some(act) = action.get("act") else {
            turn = "Reply with ONE JSON object of form {\"act\":{\"target\":..,\"payload\":..}}."
                .into();
            continue;
        };
        acts += 1;
        let target = act
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let payload = act.get("payload").cloned().unwrap_or(json!({}));
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");

        // Note the discovery milestones.
        if target == "kernel" && verb == "reflect" {
            reflected_kernel = true;
        }

        let result = kernel
            .send(&AgentId::from(target.as_str()), payload.clone())
            .await;

        if target == "kernel"
            && verb == "reflect"
            && (result.get("self_extension").is_some()
                || result.to_string().contains("self_extension"))
        {
            saw_self_extension = true;
        }
        // A successful create_agent on ANY script executor (js / lua / python)?
        let hm = payload
            .get("handler_module")
            .and_then(Value::as_str)
            .unwrap_or("");
        if verb == "create_agent"
            && (hm == "js_runtime.tools"
                || hm == "lua_runtime.tools"
                || hm == "python_runtime.tools")
            && result.get("error").is_none()
        {
            let id = result
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("greeter")
                .to_string();
            created_greeter = true;
            if !watching {
                kernel
                    .watch(&AgentId::from(id.as_str()), AgentId::from("seed_watch"))
                    .await;
                watching = true;
            }
        }

        // Feed the reply back (trimmed) as the next turn.
        let mut shown = result.to_string();
        if shown.len() > 8000 {
            shown.truncate(8000);
            shown.push_str("…(truncated)");
        }
        turn = format!("Result of your send to '{target}':\n{shown}");

        // Did the authored tool already echo the probe? (drain the watch)
        while let Ok(ev) = wrx.try_recv() {
            if ev.get("type").and_then(Value::as_str) == Some("say")
                && ev
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.contains(PROBE_TEXT))
            {
                println!("greeter → echoed: {ev}");
                created_greeter = true;
                break 'outer;
            }
        }
    }

    // Give any in-flight echo a moment to land after the loop.
    let mut echoed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), wrx.recv()).await {
            Ok(Some(ev)) => {
                if ev.get("type").and_then(Value::as_str) == Some("say")
                    && ev
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.contains(PROBE_TEXT))
                {
                    println!("greeter → echoed: {ev}");
                    echoed = true;
                    break;
                }
            }
            _ => break,
        }
    }

    println!(
        "\n════ VERDICT ════\nacts: {acts} · reflected_kernel: {reflected_kernel} · saw_self_extension: {saw_self_extension} · created_greeter: {created_greeter} · echoed_probe: {echoed}"
    );
    assert!(
        reflected_kernel && saw_self_extension,
        "reflect-first: the model must reflect kernel and encounter self_extension"
    );
    assert!(
        created_greeter,
        "the model must AUTHOR a js_runtime tool (greeter)"
    );
    assert!(
        echoed,
        "the authored tool must actually run and echo the probe text"
    );
}

/// GOAL 2 — a NON-LOOP shape: the authored tool must itself `create_agent` a fresh
/// worker per turn (tree/decomposition). Falsifiable: after driving, a script
/// agent exists that the MODEL never directly created — the tool spawned it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live model call — set FANTASTIC_AI_MODEL (local ollama → Ollama Cloud)"]
async fn seed_reflect_first_tree_spawns_worker() {
    let Some(model) = env("FANTASTIC_AI_MODEL") else {
        eprintln!("SKIP: set FANTASTIC_AI_MODEL");
        return;
    };
    let endpoint =
        env("FANTASTIC_OLLAMA_ENDPOINT").unwrap_or_else(|| "http://localhost:11434".into());

    let tmp = tempfile::tempdir().unwrap();
    let mut kernel = fantastic_kernel::Kernel::new();
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    kernel.bundles.register(
        "ollama_backend.tools",
        fantastic_ollama_backend::OllamaBackendBundle,
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
    // Seed the root manual (SHORT orientation + the keyed `docs/` tree) to disk so
    // the model can `reflect readme=index` / `readme=<key>` and the root reflect's
    // `readmes` index is populated — the harness builds the kernel directly (no CLI
    // bootstrap), so nothing else seeds it.
    let _ = fantastic_core::seed_root_readme(tmp.path());
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
            json!({"type":"create_agent","handler_module":"ollama_backend.tools","id":ORGAN,
                   "file_bridge_id":"ff_live", "model": model, "endpoint": endpoint}),
        )
        .await;
    assert!(r.get("error").is_none(), "create organ: {r:#?}");

    let task = "TASK: Author a DURABLE tool named 'super' that implements a TREE / DECOMPOSITION \
        control flow — NOT a simple receive-loop. Whenever 'super' receives a turn \
        {type:'send', text, client_id}, its OWN code must CREATE A BRAND-NEW worker agent (by \
        sending 'kernel' a create_agent whose `code` you also write) to handle THAT turn — the \
        worker reverses the text and emits it — then 'super' relays the result as say+done. So \
        super's code itself calls create_agent: scripts creating scripts, a fresh worker per \
        turn. Discover the executor + script API by REFLECTING first. After 'super' exists, send \
        it {\"type\":\"send\",\"text\":\"abcde\",\"client_id\":\"me\"}. Reply {\"final\":...} once you have sent that."
        .to_string();

    let base = ["core", "ff_live", ORGAN];
    let mut model_created: Vec<String> = Vec::new();
    let mut turn = task;
    let mut reflected_kernel = false;
    let mut saw_self_extension = false;
    let mut created_super = false;
    let mut acts = 0usize;

    for step in 0..18usize {
        let reply = kernel
            .send(
                &AgentId::from(ORGAN),
                json!({"type":"send","text": turn, "system_prompt": LEAN_SYSTEM, "client_id":"explorer"}),
            )
            .await;
        let resp = reply["response"].as_str().unwrap_or("");
        if let Some(err) = reply.get("error").and_then(Value::as_str) {
            panic!("organ error at step {step}: {err}");
        }
        println!("\n─── step {step} · model ───\n{resp}\n");
        let Some(action) = first_json(resp) else {
            turn = "Reply with ONE JSON object: {\"act\":{...}} or {\"final\":...}.".into();
            continue;
        };
        if action.get("final").is_some() {
            println!("model → final: {}", action["final"]);
            break;
        }
        let Some(act) = action.get("act") else {
            turn = "Reply with ONE JSON object {\"act\":{\"target\":..,\"payload\":..}}.".into();
            continue;
        };
        acts += 1;
        let target = act
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let payload = act.get("payload").cloned().unwrap_or(json!({}));
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if target == "kernel" && verb == "reflect" {
            reflected_kernel = true;
        }
        let result = kernel
            .send(&AgentId::from(target.as_str()), payload.clone())
            .await;
        if target == "kernel" && verb == "reflect" && result.to_string().contains("self_extension")
        {
            saw_self_extension = true;
        }
        let hm = payload
            .get("handler_module")
            .and_then(Value::as_str)
            .unwrap_or("");
        if verb == "create_agent"
            && (hm.ends_with("_runtime.tools"))
            && result.get("error").is_none()
        {
            let id = result
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            model_created.push(id);
            created_super = true;
        }
        let mut shown = result.to_string();
        if shown.len() > 8000 {
            shown.truncate(8000);
            shown.push_str("…(truncated)");
        }
        turn = format!("Result of your send to '{target}':\n{shown}");
    }
    // Let a spawned worker land.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Did the TOOL spawn a sub-agent the model never directly created?
    let listed = kernel
        .send(&AgentId::from("kernel"), json!({"type":"list_agents"}))
        .await;
    let mut tool_spawned: Option<String> = None;
    if let Some(agents) = listed["agents"].as_array() {
        for a in agents {
            let id = a.get("id").and_then(Value::as_str).unwrap_or("");
            let hm = a
                .get("handler_module")
                .and_then(Value::as_str)
                .unwrap_or("");
            if hm.ends_with("_runtime.tools")
                && !base.contains(&id)
                && !model_created.iter().any(|m| m == id)
            {
                tool_spawned = Some(id.to_string());
            }
        }
    }
    println!(
        "\n════ VERDICT (goal 2 · tree) ════\nacts: {acts} · reflected_kernel: {reflected_kernel} · saw_self_extension: {saw_self_extension} · created_super: {created_super} · tool_spawned_worker: {tool_spawned:?}"
    );
    assert!(
        reflected_kernel && saw_self_extension,
        "reflect-first: the model must reflect kernel and encounter self_extension"
    );
    assert!(created_super, "the model must AUTHOR the supervisor tool");
    assert!(
        tool_spawned.is_some(),
        "NON-LOOP tree shape: the authored tool must itself create_agent a worker (a sub-agent the model never directly created)"
    );
}

/// GENERIC AGENCY / "mind" layer v0 — verify-and-repair. Same reflect-first
/// driver, but it REFUSES the model's `final` until the authored tool actually
/// echoes: on a premature final it reflects the tool's real state and feeds it
/// back with a generic "diagnose and fix" nudge, looping. Model-agnostic agency
/// (observe outcome → push back on false completion → let the model repair) —
/// NOT task/API scaffolding. Tests whether this lifts a sub-frontier model to a
/// WORKING tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live model call — set FANTASTIC_AI_MODEL (local ollama → Ollama Cloud)"]
async fn mind_verify_repair_author_and_use() {
    let Some(model) = env("FANTASTIC_AI_MODEL") else {
        eprintln!("SKIP: set FANTASTIC_AI_MODEL");
        return;
    };
    let endpoint =
        env("FANTASTIC_OLLAMA_ENDPOINT").unwrap_or_else(|| "http://localhost:11434".into());

    let tmp = tempfile::tempdir().unwrap();
    let mut kernel = fantastic_kernel::Kernel::new();
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    kernel.bundles.register(
        "ollama_backend.tools",
        fantastic_ollama_backend::OllamaBackendBundle,
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
    // Seed the root manual (SHORT orientation + the keyed `docs/` tree) to disk so
    // the model can `reflect readme=index` / `readme=<key>` and the root reflect's
    // `readmes` index is populated — the harness builds the kernel directly (no CLI
    // bootstrap), so nothing else seeds it.
    let _ = fantastic_core::seed_root_readme(tmp.path());
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
            json!({"type":"create_agent","handler_module":"ollama_backend.tools","id":ORGAN,
                   "file_bridge_id":"ff_live", "model": model, "endpoint": endpoint}),
        )
        .await;
    assert!(r.get("error").is_none(), "create organ: {r:#?}");

    let (wtx, mut wrx) = tokio::sync::mpsc::channel::<Value>(1024);
    kernel.inboxes.insert(AgentId::from("seed_watch"), wtx);

    let task = format!(
        "TASK: Author a DURABLE tool that echoes text. When it receives {{type:'send', text, \
         client_id}} it must emit a 'say' with that text (tagged client_id) then a 'done'. \
         Discover the executor + script API by REFLECTING first. Create it, then send it \
         {{\"type\":\"send\",\"text\":\"{PROBE_TEXT}\",\"client_id\":\"me\"}}. Only reply final once it \
         actually echoes.",
    );

    let mut turn = task;
    let mut reflected_kernel = false;
    let mut saw_self_extension = false;
    let mut created_any = false;
    let mut echoed = false;
    let mut last_tool: Option<String> = None;
    let mut acts = 0usize;
    let mut repairs = 0usize;

    let drain = |wrx: &mut tokio::sync::mpsc::Receiver<Value>| -> bool {
        let mut hit = false;
        while let Ok(ev) = wrx.try_recv() {
            if ev.get("type").and_then(Value::as_str) == Some("say")
                && ev
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.contains(PROBE_TEXT))
            {
                println!("tool → echoed: {ev}");
                hit = true;
            }
        }
        hit
    };

    for step in 0..26usize {
        let reply = kernel
            .send(
                &AgentId::from(ORGAN),
                json!({"type":"send","text": turn, "system_prompt": LEAN_SYSTEM, "client_id":"explorer"}),
            )
            .await;
        let resp = reply["response"].as_str().unwrap_or("");
        if let Some(err) = reply.get("error").and_then(Value::as_str) {
            panic!("organ error at step {step}: {err}");
        }
        println!("\n─── step {step} · model ───\n{resp}\n");

        // Late echoes may have landed since last turn.
        if drain(&mut wrx) {
            echoed = true;
        }

        let Some(action) = first_json(resp) else {
            turn = "Reply with ONE JSON object: {\"act\":{...}} or {\"final\":...}.".into();
            continue;
        };

        if action.get("final").is_some() {
            tokio::time::sleep(Duration::from_millis(400)).await;
            if drain(&mut wrx) {
                echoed = true;
            }
            if echoed {
                println!("model → final (verified): {}", action["final"]);
                break;
            }
            // REFUSE the false completion — reflect the tool + push back.
            repairs += 1;
            let state = if let Some(tid) = &last_tool {
                kernel
                    .send(&AgentId::from(tid.as_str()), json!({"type":"reflect"}))
                    .await
                    .to_string()
            } else {
                "you have not created any tool yet".to_string()
            };
            let mut state = state;
            if state.len() > 1500 {
                state.truncate(1500);
                state.push('…');
            }
            turn = format!(
                "You replied final, but I have NOT observed your tool echo \"{PROBE_TEXT}\" — you have \
                 NOT succeeded. Do not give up. Current state of your tool: {state}. DIAGNOSE and FIX: \
                 is it running? did its code error? correct the `code` (update_agent then stop+boot, or \
                 delete_agent then create_agent again with fixed code), then send it \
                 {{\"type\":\"send\",\"text\":\"{PROBE_TEXT}\",\"client_id\":\"me\"}} again. Emit your next act now."
            );
            continue;
        }

        let Some(act) = action.get("act") else {
            turn = "Reply with ONE JSON object {\"act\":{\"target\":..,\"payload\":..}}.".into();
            continue;
        };
        acts += 1;
        let target = act
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let payload = act.get("payload").cloned().unwrap_or(json!({}));
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if target == "kernel" && verb == "reflect" {
            reflected_kernel = true;
        }
        let result = kernel
            .send(&AgentId::from(target.as_str()), payload.clone())
            .await;
        if target == "kernel" && verb == "reflect" && result.to_string().contains("self_extension")
        {
            saw_self_extension = true;
        }
        let hm = payload
            .get("handler_module")
            .and_then(Value::as_str)
            .unwrap_or("");
        if verb == "create_agent" && hm.ends_with("_runtime.tools") && result.get("error").is_none()
        {
            if let Some(id) = result.get("id").and_then(Value::as_str) {
                created_any = true;
                last_tool = Some(id.to_string());
                kernel
                    .watch(&AgentId::from(id), AgentId::from("seed_watch"))
                    .await;
            }
        }
        if drain(&mut wrx) {
            echoed = true;
        }
        let mut shown = result.to_string();
        if shown.len() > 8000 {
            shown.truncate(8000);
            shown.push_str("…(truncated)");
        }

        // AUTONOMOUS VERIFY (the mind's core): once a tool exists, the driver
        // itself probes it every turn and reports GROUND TRUTH — so the model
        // repairs against reality, not its own optimism. Generic agency.
        let mut verify_note = String::new();
        if let Some(tid) = last_tool.clone() {
            if !echoed {
                kernel
                    .send(
                        &AgentId::from(tid.as_str()),
                        json!({"type":"send","text":PROBE_TEXT,"client_id":"me"}),
                    )
                    .await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                if drain(&mut wrx) {
                    echoed = true;
                }
                if echoed {
                    verify_note = format!(
                        "\n\nAUTONOMOUS VERIFY: I sent your tool '{tid}' the probe and it CORRECTLY echoed \"{PROBE_TEXT}\" — it works. You may reply final."
                    );
                } else {
                    repairs += 1;
                    let st = kernel
                        .send(&AgentId::from(tid.as_str()), json!({"type":"reflect"}))
                        .await
                        .to_string();
                    let st: String = st.chars().take(1200).collect();
                    verify_note = format!(
                        "\n\nAUTONOMOUS VERIFY: I sent your tool '{tid}' {{\"type\":\"send\",\"text\":\"{PROBE_TEXT}\",\"client_id\":\"me\"}} and it emitted NO say/done — it does NOT work yet. Its state: {st}. Its code MUST: receive a turn where type=='send', read `text` and `client_id`, then kernel.emit({{type:'say', text:<the text>, client_id:<client_id>}}) then kernel.emit({{type:'done', client_id:<client_id>}}). Fix it: update_agent its `code` then stop+boot, or delete_agent then create_agent again with corrected code."
                    );
                }
            }
        }
        if echoed {
            println!("tool verified working at step {step}");
            break;
        }
        turn = format!("Result of your send to '{target}':\n{shown}{verify_note}");
    }

    println!(
        "\n════ VERDICT (mind · verify+repair) ════\nacts: {acts} · repairs: {repairs} · reflected_kernel: {reflected_kernel} · saw_self_extension: {saw_self_extension} · created_any: {created_any} · echoed: {echoed}"
    );
    assert!(
        reflected_kernel && saw_self_extension,
        "must discover self_extension"
    );
    assert!(created_any, "must author a tool");
    assert!(
        echoed,
        "with verify+repair, the tool must ultimately echo the probe"
    );
}
