//! A chat paused before a restart is `paused` in the sidebar after it (#714): the shell boots
//! the sidebar first, so its tap on the kernel's state stream hears the `paused` each chat
//! says at its own boot when its store says paused.
//!
//! The chat sits directly under the root with an id that sorts before `sidebar`, so the disk
//! walk registers it first: only the boot order puts the sidebar ahead of it.
//!
//! Its own integration binary: the bundles clamp against the process cwd and the user home is
//! an env var, both process-global (see `boot_kernel_public.rs`).

use std::path::PathBuf;
use std::sync::Arc;

use fantastic_kernel::{AgentId, Item, Kernel};
use fantastic_tui::{boot_kernel, seed_dir_with, stop_kernel};
use serde_json::{json, Value};

const CHAT: &str = "c714";

/// A chat on a provider whose model nobody serves: pause and resume never call the model.
/// The stock tree seeds no sidebar (its bundle stays): this blueprint places one at the root.
const BLUEPRINT: &str = "\
agents:
  - id: sidebar
    handler_module: sidebar.tools
  - id: c714
    handler_module: ai_chat.tools
    provider_id: p714
    memory_store_id: memory
    can:
      memory: {call: [append, read]}
      p714: {call: [\"*\"]}
  - id: p714
    handler_module: vllm_backend.tools
    endpoint: http://127.0.0.1:9/v1
    model: stub
";

fn folder(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fc-sidebar-first-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn call(k: &Arc<Kernel>, to: &str, payload: Value) -> Value {
    fantastic_kernel::test_support::send_settled(k, vec![Item::new(AgentId::from(to), payload)]).await.remove(0).result
}

/// The phases the sidebar holds once it holds one for `CHAT` (the tap publishes off the call
/// that emitted), or what it holds after 2 s.
async fn phase_of(k: &Arc<Kernel>) -> Value {
    let mut last = Value::Null;
    for _ in 0..100 {
        last = call(k, "sidebar", json!({"type": "reflect"})).await["phases"].clone();
        if last.get(CHAT).is_some() {
            return last;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    last
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_paused_before_a_restart_is_paused_in_the_sidebar_after_it() {
    let home = folder("home");
    std::env::set_var("FANTASTIC_HOME", &home);
    let dir = folder("dir");
    assert_eq!(seed_dir_with(&dir.join(".fantastic"), &[BLUEPRINT]), Ok(true));
    std::env::set_current_dir(&dir).unwrap();
    let state = dir.join(".fantastic");

    let k = boot_kernel(&state, &dir, &[], None, None).await.expect("boots");
    let order = k.agent_ids();
    let at = |id: &str| order.iter().position(|a| a.as_str() == id).unwrap_or_else(|| panic!("`{id}` hydrated"));
    assert!(at(CHAT) < at("sidebar"), "the chat is registered ahead of the sidebar: {order:?}");
    let r = call(&k, CHAT, json!({"type": "pause"})).await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(phase_of(&k).await, json!({CHAT: "paused"}), "paused while running");
    stop_kernel(&k).await;

    let k = boot_kernel(&state, &dir, &[], None, None).await.expect("boots again");
    assert_eq!(phase_of(&k).await, json!({CHAT: "paused"}), "the sidebar heard the chat's boot");
    stop_kernel(&k).await;

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    for p in [&dir, &home] {
        let _ = std::fs::remove_dir_all(p);
    }
}
