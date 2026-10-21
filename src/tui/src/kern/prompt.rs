//! THE PROJECT'S SYSTEM BLOCK: `.fantastic/prompt.md` (owner 2026-10-08:
//! "bundled default, seeded, then replace if defined, no appends").
//!
//! - SEEDED when missing, at every boot, with [`DEFAULT`] — ai_core's own block
//!   written in its placeholders (`{tree}\n\n{self}\n\n{howto}`, ai_core's
//!   default), then the CLI's building rules — until the human edits it. A
//!   human, or a coding CLI reading the folder, sees and changes it.
//! - Every AI agent READS it, as itself, through [`BRIDGE`] (a read-only
//!   file_bridge exposing only that file): its record names `file_bridge_id:
//!   prompt_file` and `system_prompt_file: .fantastic/prompt.md` — stamped on
//!   every AI that names neither,
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

fn exists(h: &Host, id: &str) -> bool {
    h.kernel
        .agents
        .contains_key(&fantastic_kernel::AgentId::from(id))
}

fn err_of(r: &Value) -> Option<String> {
    r.get("error").and_then(Value::as_str).map(str::to_string)
}

/// Seed the file when missing; make (and boot) the bridge when missing.
pub async fn ensure(h: &Host, cwd: &Path) -> Result<(), String> {
    let f = cwd.join(".fantastic").join(FILE);
    if !f.exists() {
        std::fs::write(&f, DEFAULT).map_err(|e| format!("{}: {e}", f.display()))?;
    }
    if !exists(h, BRIDGE) {
        let r = h
            .operator(
                "kernel",
                json!({"type": "create_agent", "parent_id": crate::env::HUMAN, "id": BRIDGE,
                "handler_module": "file_bridge.tools", "root": ".", "readonly": true,
                "allow": [PATH], "ingress_rule": "allow_all_ingress"}),
            )
            .await
            .result;
        if let Some(e) = err_of(&r) {
            return Err(format!("the prompt bridge `{BRIDGE}`: {e}"));
        }
    }
    Ok(())
}

/// Whether `id` is an AI agent (its bundle's record takes a memory store).
fn is_ai(h: &Host, id: &str) -> bool {
    let Some(a) = h.kernel.agents.get(&fantastic_kernel::AgentId::from(id)) else {
        return false;
    };
    let r = a.value().record();
    let Some(hm) = r.handler_module.as_deref() else {
        return false;
    };
    r.meta
        .get("memory_store_id")
        .and_then(Value::as_str)
        .is_some()
        && crate::kern::backend::BackendBundle::is_ai_handler(h, hm)
}

/// Name the file on AI `id`'s record — unless it names a file, or a bridge, of
/// its own (a bridge alone means the default block: left as it is).
pub async fn stamp(h: &Host, id: &str) -> Result<(), String> {
    if !exists(h, BRIDGE) || !is_ai(h, id) {
        return Ok(());
    }
    let own = h
        .kernel
        .agents
        .get(&fantastic_kernel::AgentId::from(id))
        .is_some_and(|a| {
            let m = &a.value().record().meta;
            ["system_prompt_file", "file_bridge_id"]
                .iter()
                .any(|k| m.get(*k).is_some_and(|v| !v.is_null()))
        });
    if own {
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
