//! LIVE — can a model AUTHOR A MIND (not just an echo tool)?
//!
//! The honest question behind the product's "Plan/execute mind" button. Today the
//! TUI PASTES a hand-written template, which is not emergence. This test asks the
//! `author` agent (the verify-repair driver, proven to lift a weak model to a
//! WORKING echo tool) to drive the brain into authoring something several rungs
//! harder: an agent that DRIVES the brain itself, reports with say/done, and hands
//! a decision back to the human as a `question` event.
//!
//! NOTHING of the API is given. The goal is a CAPABILITY, stated the way a human
//! would state it; every shape the model needs (the organ's `send`, the `say`/`done`
//! and `question` events, the script API) is in the kernel's OWN docs — it must
//! reflect them. What must EMERGE is the whole thing: the discovery AND the loop.
//!
//! And the check has teeth — TWO sets, because the first set was gamed twice:
//!
//! * `must_call: "brain"`: the authored agent must be OBSERVED driving the
//!   language-model organ while it answers the probe. An echo answers a probe
//!   perfectly — and an echo FAILS this, and is pushed into repair with that
//!   diagnosis.
//! * `expect: "BANANA"` — THE CANARY, and the check that actually decides. `must_call`
//!   asserts a MECHANISM, and a mechanism can be satisfied without doing the job: the
//!   last model to "pass" it called the brain with an INVENTED verb, got
//!   `{"error":"ollama: unknown type \"prompt\""}` back, and served that error string
//!   as its answer. It touched the brain; it never THOUGHT. So the probe is now a turn
//!   whose only correct handling puts the MODEL's completion in the agent's mouth —
//!   "Reply with exactly the word BANANA and nothing else." — and the agent's `say`
//!   must carry the tracer. An echo cannot produce it, an error-relay cannot, a
//!   wrong-verb call cannot. Only a mind that hands the turn to the organ and USES
//!   what comes back can. Success is never the model's word, and now it cannot be a
//!   mechanism's word either.
//!
//! Falsifiable: `{id, code}` back ⇒ a real, running, THINKING mind the model wrote.
//! `{error: gave up …}` ⇒ it cannot, and we say so.
//!
//! Env-gated: FANTASTIC_AI_MODEL (e.g. gemma4:12b), FANTASTIC_OLLAMA_ENDPOINT.
//!   cargo test -p fantastic-host --test author_mind_live -- --ignored --nocapture

use fantastic_kernel::AgentId;
use serde_json::{json, Value};
use std::sync::Arc;

/// The MIND's goal — a CAPABILITY, not an API listing. Not one send/emit signature
/// appears here on purpose: the kernel self-describes (its `minds` and
/// `self-extension` docs carry every shape), and DISCOVERING that is half the task.
/// Spoon-feeding the protocol would be dictation, and dictation is not emergence.
const MIND_GOAL: &str = r#"Author a durable agent that works on the user's behalf — a MIND — and make it genuinely work.

When the user gives it a task it must actually THINK about it, not echo it back: this kernel contains a language-model organ (the agent "brain"), and the mind's job is to use it — hand it the user's turn, read what comes back, and act on it. The organ does one pass and has no loop of its own, so the control flow around it is YOURS to write.

It must report what it did, so a human can watch it work. And when a choice is genuinely the human's to make, it must hand that decision back to them and wait for their answer, rather than guessing.

Discover what this kernel offers and how — its agents, its verbs, its events, how a durable agent is authored and what its script may do — by REFLECTING it. Everything you need is documented inside the system. Then build the mind as a durable agent whose persisted code is a SCRIPT the kernel runs top to bottom, and prove it works."#;

/// THE CANARY PROBE. Its only correct handling is: hand this turn to the organ, and
/// say what the organ says. Then the tracer appears — and it appears NOWHERE else.
const PROBE: &str = "Reply with exactly the word BANANA and nothing else.";
const CANARY: &str = "BANANA";

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

/// Mirror everything flowing through `id` to stdout under `tag` — a read-only tap
/// (a synthetic watcher), so the transcript of a live run is inspectable.
async fn spy(kernel: &Arc<fantastic_kernel::Kernel>, id: &str, tag: &'static str) {
    let ear = AgentId::from(format!("spy__{id}").as_str());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(4096);
    kernel.inboxes.insert(ear.clone(), tx);
    kernel.watch(&AgentId::from(id), ear).await;
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            // Token spam from the backend would bury the tape.
            if ev.get("type").and_then(Value::as_str) == Some("token") {
                continue;
            }
            // chars(), not truncate() — the turns are full of em-dashes, and a
            // byte-truncation lands mid-codepoint and kills the tape.
            let s: String = ev.to_string().chars().take(2400).collect();
            println!("── {tag} ▸ {s}");
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live model call — set FANTASTIC_AI_MODEL"]
async fn author_agent_emerges_a_working_mind() {
    let Some(model) = env("FANTASTIC_AI_MODEL") else {
        eprintln!("SKIP: set FANTASTIC_AI_MODEL (e.g. gemma4:12b)");
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
    kernel
        .bundles
        .register(fantastic_author::HANDLER_MODULE, fantastic_author::AuthorBundle);
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
    let _ = fantastic_core::seed_root_readme(tmp.path());

    kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"file_bridge.tools","id":"ff_live",
                   "root": tmp.path().join(".fantastic").to_string_lossy(), "ingress_rule":"allow_all"}),
        )
        .await;
    let brain = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"ollama_backend.tools","id":"brain",
                   // num_ctx: ollama's stock default is 4096, which OOMs before a
                   // single turn once a mind sends a real system_prompt — the last
                   // run's authored mind called the organ CORRECTLY and got back
                   // `context_insufficient`. That is our misconfiguration, not the
                   // model's failure. Give the brain a real window.
                   "file_bridge_id":"ff_live", "model": model, "endpoint": endpoint,
                   "num_ctx": 32768, "think": false}),
        )
        .await;
    assert_eq!(brain["id"], "brain", "brain must exist: {brain}");
    let a = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"author.tools","id":"author",
                   "brain_id":"brain"}),
        )
        .await;
    assert_eq!(a["id"], "author", "author agent must exist: {a}");

    // ── the tape: watch the run so a FAILURE is readable, not a shrug. Every act
    // the model dispatches lands on the root ('kernel' resolves to it); every turn
    // the author feeds the model lands on the brain; the author narrates itself.
    spy(&kernel, "core", "ACT").await;
    spy(&kernel, "brain", "TURN").await;
    spy(&kernel, "author", "MIND").await;

    // Drive it. The author agent only returns an id once it has OBSERVED the authored
    // mind ANSWER the canary probe with the tracer in its mouth (and drive the brain
    // while doing it) — success is never the model's say-so, an echo is not a mind,
    // and neither is an agent that stringifies the kernel's error and calls it an
    // answer.
    let r: Value = kernel
        .send(
            &AgentId::from("author"),
            json!({"type":"author","goal": MIND_GOAL, "probe": PROBE, "expect": CANARY,
                   // The budget is a RESOURCE, not a check — raising it weakens
                   // nothing (canary + must_call still decide). The 24-step run cut
                   // gpt-oss off one turn after it first compiled AND had just
                   // fetched the `minds` doc off the echo push-back, so 24 measured
                   // our patience, not the model. Env-tunable so a re-test is honest.
                   "must_call":"brain",
                   "steps": env("FANTASTIC_AUTHOR_STEPS")
                       .and_then(|s| s.parse::<u64>().ok())
                       .unwrap_or(24)}),
        )
        .await;

    println!("\n═══ author → {}\n", serde_json::to_string_pretty(&r).unwrap());

    if let Some(err) = r.get("error").and_then(Value::as_str) {
        panic!("the model could NOT author a working mind: {err}");
    }
    let id = r["id"].as_str().expect("authored mind id");
    let code = r["code"].as_str().unwrap_or("");
    println!("═══ the model authored `{id}` ═══\n{code}\n");
    assert!(
        !code.trim().is_empty(),
        "the authored mind must carry real code"
    );
    // It answered the probe AND was observed driving the brain (the author agent
    // verified both), so it is a LIVING, THINKING agent the model wrote — not a
    // template we pasted, and not an echo.
    assert!(
        code.contains("on_message"),
        "a mind must serve its inbox (on_message loop); got:\n{code}"
    );
    // Cheap static echo of the behavioural check `must_call` already enforced.
    assert!(
        code.contains("brain"),
        "a mind must reach the language-model organ; got:\n{code}"
    );

    // ── AND, INDEPENDENTLY: probe the authored mind OURSELVES, exactly as
    // `authored_mind_probe.rs` does. The pass must not rest on the driver's own
    // bookkeeping — we send the artifact a fresh turn, listen on its id, and read what
    // it says with our own eyes. A second, different canary, so a mind that somehow
    // memorised the first one is caught here.
    let ear = AgentId::from("probe_ear");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(256);
    kernel.inboxes.insert(ear.clone(), tx);
    kernel.watch(&AgentId::from(id), ear).await;
    kernel
        .send(
            &AgentId::from(id),
            json!({"type":"send","text":"Reply with exactly the word PLATYPUS and nothing else.",
                   "client_id":"probe"}),
        )
        .await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut said: Option<String> = None;
    while tokio::time::Instant::now() < deadline && said.is_none() {
        while let Ok(ev) = rx.try_recv() {
            if ev.get("type").and_then(Value::as_str) == Some("say") {
                said = Some(ev["text"].as_str().unwrap_or("").to_string());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let said = said.expect("the authored mind never answered our own probe");
    println!("\n═══ WE ASKED `{id}` FOR PLATYPUS AND IT SAID ═══\n{said}\n");
    assert!(
        said.to_uppercase().contains("PLATYPUS"),
        "the authored mind does not really think — our own probe got:\n{said}"
    );
}
