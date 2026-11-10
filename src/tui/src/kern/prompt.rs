//! THE PROJECT'S SYSTEM BLOCK: `.fantastic/prompt.md` (owner 2026-10-08:
//! "bundled default, seeded, then replace if defined, no appends").
//!
//! - SEEDED when missing, at every boot, with [`DEFAULT`] — ai_core's own block
//!   written in its placeholders (`{tree}\n\n{self}\n\n{howto}`, ai_core's
//!   default), then the CLI's building rules — until the human edits it. A
//!   human, or a coding CLI reading the folder, sees and changes it.
//! - Every backend agent READS it, as itself, through [`BRIDGE`] (a read-only
//!   file_bridge exposing only that file): its record names `file_bridge_id:
//!   prompt_file` and `system_prompt_file: .fantastic/prompt.md` — stamped on
//!   every backend agent that names neither,
//!   at boot and as each is made (`kern/reach` hands every AI the bridge's
//!   verbs, both edges). The lib watches the file (`watch_path`): a save is the
//!   block from the next turn on, no restart — the KV cache rebuilds once, on
//!   the human's own act. An AI's own `system_prompt` beats the file; nothing is
//!   appended. A missing or EMPTY file FAILS the turn loud (the lib: no fallback
//!   to the default block) — the file is re-seeded at the next boot.

use std::path::Path;

use serde_json::{json, Value};

use crate::kern::host::Host;

/// The file, under `.fantastic/`.
pub const FILE: &str = "prompt.md";
/// Its path under the bridge's root (the project dir — NOT `.fantastic`: a
/// file_bridge rooted at the kernel's own store is taken as the place its
/// records persist through, and a read-only one refuses every write).
pub const PATH: &str = ".fantastic/prompt.md";
/// The read-only bridge every AI reads it through.
pub const BRIDGE: &str = "prompt_file";
/// What a fresh project's file holds: ai_core's default block, as placeholders,
/// then the CLI's building rules (owner 2026-10-16: agents through `create_agent`,
/// a new bundle only when the user asks, `fit` an html cell after placing it).
pub const DEFAULT: &str = "{tree}\n\n{self}\n\n{howto}\n\n## Building here\n- Make every agent with a `create_agent` call to `kernel`, from the bundles reflect lists.\n- Write a new bundle (`.fantastic/bundles/<name>/`, or `spawn` with `bundle`) ONLY when the user asks for one.\n- After placing or updating an html cell, size it to its content: `fit {id}` on its view.\n";

fn err_of(r: &Value) -> Option<String> {
    r.get("error").and_then(Value::as_str).map(str::to_string)
}

/// Seed the file when missing; make (and boot) the bridge when missing.
pub async fn ensure(h: &Host, cwd: &Path) -> Result<(), String> {
    let f = cwd.join(".fantastic").join(FILE);
    if !f.exists() {
        std::fs::write(&f, DEFAULT).map_err(|e| format!("{}: {e}", f.display()))?;
    }
    if !h.kernel.exists(BRIDGE) {
        let r = h
            .operator("kernel", json!({"type": "create_agent", "parent_id": crate::env::HUMAN, "id": BRIDGE,
                "handler_module": "file_bridge.tools", "root": ".", "readonly": true,
                "allow": [PATH], "ingress_rule": "allow_all_ingress"}))
            .await
            .result;
        if let Some(e) = err_of(&r) {
            return Err(format!("the prompt bridge `{BRIDGE}`: {e}"));
        }
    }
    Ok(())
}

/// Name the file on AI `id`'s record — unless it names a file, or a bridge, of
/// its own (a bridge alone means the default block: left as it is).
pub async fn stamp(h: &Host, id: &str) -> Result<(), String> {
    if !h.kernel.exists(BRIDGE) {
        return Ok(());
    }
    // None: not a chat (or gone): nothing to stamp. Some(own): names a file or bridge already.
    let own = h.kernel.agents.get(&fantastic_kernel::AgentId::from(id)).and_then(|a| {
        let r = a.value().record();
        (r.handler_module.as_deref() == Some(fantastic_ai_core::chat_bundle::HANDLER_MODULE)).then(|| ["system_prompt_file", "file_bridge_id"].iter().any(|k| r.meta.get(*k).is_some_and(|v| !v.is_null())))
    });
    if own != Some(false) {
        return Ok(());
    }
    let r = h
        .operator("kernel", json!({"type": "update_agent", "id": id, "file_bridge_id": BRIDGE, "system_prompt_file": PATH}))
        .await
        .result;
    err_of(&r).map_or(Ok(()), |e| Err(format!("{id}: system_prompt_file: {e}")))
}

/// [`stamp`] every AI agent now.
pub async fn stamp_all(h: &Host) -> Vec<String> {
    let ids: Vec<String> = h.kernel.agents.iter().map(|a| a.key().0.clone()).collect();
    let mut notes = Vec::new();
    for id in ids {
        if let Err(e) = stamp(h, &id).await {
            notes.push(e);
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use fantastic_kernel::{Agent, AgentId, Kernel};
    use serde_json::Map;
    use std::sync::Arc;

    #[tokio::test]
    async fn stamp_names_the_prompt_file_on_a_chat_only() {
        let dir = std::env::temp_dir().join(format!("fc-prompt-{}-stamp", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
            state_dir: dir.join(".fantastic"),
            work_dir: dir.to_path_buf(),
        });
        kernel.bundles.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
        let kernel = Arc::new(kernel);
        let root = Agent::new(
            AgentId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            Map::new(),
            dir.join(".fantastic"),
            false,
        );
        kernel.register(Arc::clone(&root));
        kernel.set_root(root);
        let put = |id: &str, handler: &str, meta: Value| {
            kernel.register(Agent::new(
                AgentId::from(id),
                Some(handler.to_string()),
                Some(AgentId::from("kernel")),
                meta.as_object().cloned().unwrap_or_default(),
                std::path::PathBuf::from("/nonexistent"),
                false,
            ));
        };
        put(BRIDGE, "probe.tools", json!({}));
        put("backend", "vllm_backend.tools", json!({}));
        put("py", "py_agent.tools", json!({"memory_store_id": "m"}));
        put("chat", "ai_chat.tools", json!({"memory_store_id": "m"}));
        let h = Host { kernel, cfg: crate::env::ClientCfg::here(), root: "kernel".to_string() };
        for id in ["backend", "py", "chat"] {
            stamp(&h, id).await.unwrap();
        }
        let file = |id: &str| {
            h.kernel.agents.get(&AgentId::from(id)).unwrap().value().record().meta.get("system_prompt_file").cloned()
        };
        assert_eq!(file("chat"), Some(json!(PATH)));
        assert_eq!(h.kernel.agents.get(&AgentId::from("chat")).unwrap().value().record().meta["file_bridge_id"], json!(BRIDGE));
        assert_eq!(file("backend"), None, "a provider holds no prompt file: it is the chat's field");
        assert_eq!(file("py"), None, "a plain handler with a store field is not stamped");
    }
}
