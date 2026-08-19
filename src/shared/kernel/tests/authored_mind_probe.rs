//! Take the code the 120b model ACTUALLY authored, run it against a real brain,
//! and print what it says. No inference — the artifact either thinks or it doesn't.
//!
//!   cargo test -p fantastic-host --test authored_mind_probe -- --ignored --nocapture

use fantastic_kernel::AgentId;
use serde_json::{json, Value};
use std::sync::Arc;

/// VERBATIM, as gpt-oss:120b-cloud wrote it (the `{type:'prompt'}` is its own).
const AUTHORED: &str = r#"const kernel = require('kernel');
async function main() {
  while (true) {
    const msg = await kernel.on_message();
    const {type, text, client_id} = msg;
    if (type === 'send') {
      const brainResp = await kernel.send('brain', {type: 'prompt', text});
      let answer = '';
      if (brainResp && typeof brainResp === 'object') {
        if (brainResp.text) answer = brainResp.text;
        else if (brainResp.answer) answer = brainResp.answer;
        else answer = JSON.stringify(brainResp);
      } else {
        answer = String(brainResp);
      }
      await kernel.emit({type: 'say', text: answer, client_id});
      await kernel.emit({type: 'done', client_id});
    }
  }
}
main();"#;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live model call — set FANTASTIC_AI_MODEL"]
async fn what_does_the_authored_mind_actually_say() {
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
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"file_bridge.tools","id":"ff",
                   "root": tmp.path().join(".fantastic").to_string_lossy(), "ingress_rule":"allow_all"}),
        )
        .await;
    kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"ollama_backend.tools","id":"brain",
                   "file_bridge_id":"ff","model":model,"endpoint":endpoint,"think":false}),
        )
        .await;
    kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"js_runtime.tools","id":"m",
                   "code": AUTHORED}),
        )
        .await;

    // Listen to the mind itself — the thing the author's ear never showed us.
    let ear = AgentId::from("ear");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(256);
    kernel.inboxes.insert(ear.clone(), tx);
    kernel.watch(&AgentId::from("m"), ear).await;

    kernel
        .send(
            &AgentId::from("m"),
            json!({"type":"send","text":"What is 2+2? Answer in one word.","client_id":"probe"}),
        )
        .await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut said: Option<String> = None;
    while tokio::time::Instant::now() < deadline && said.is_none() {
        while let Ok(ev) = rx.try_recv() {
            if ev.get("type").and_then(Value::as_str) == Some("say") {
                said = Some(ev["text"].as_str().unwrap_or("").to_string());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let said = said.expect("the authored mind never said anything");
    println!("\n═══ THE AUTHORED MIND SAID ═══\n{said}\n══════════════════════════════\n");

    // The verdict. A mind that THINKS returns the model's answer. A mind that only
    // *appears* to call the brain returns the kernel's error, stringified.
    assert!(
        !said.contains("unknown type") && !said.contains("\"error\""),
        "FALSE PASS: the authored mind never actually thought — it relayed the kernel's \
         error back as its answer:\n{said}"
    );
}
