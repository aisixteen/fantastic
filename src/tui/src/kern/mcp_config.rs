//! THE PROJECT'S `.mcp.json` — kept by the `mcp` leg itself (owner 2026-10-08).
//!
//! A coding CLI opened in the project (Claude Code reads `.mcp.json` at the
//! project root) should find THIS kernel: `mcpServers.fantastic = {command: <this
//! binary>, args: [mcp]}`. Only the `mcp` agent writes that file, through its
//! own bridge (`fantastic-web-mcp`: on its boot, read → set its entry → write if
//! changed, every other server kept). So, at every boot, the CLI makes:
//!
//! - [`BRIDGE`]: a WRITABLE file_bridge rooted at the project, exposing only
//!   [`FILE`] (open ingress) — never rooted at `.fantastic` (the kernel would
//!   persist its records through it);
//! - the leg itself, when missing (it used to wait for the first connection —
//!   the file must be right BEFORE any client connects), naming
//!   `file_bridge_id` + `mcp_config_file`; `kern/reach` grants both edges;
//! - then boots the leg again, and reports its `mcp_config_error`, if any.

use serde_json::{json, Value};

use crate::kern::host::Host;

/// The bridge the leg writes through.
pub const BRIDGE: &str = "mcp_config";
/// The file, at the project root.
pub const FILE: &str = ".mcp.json";

fn exists(h: &Host, id: &str) -> bool {
    h.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(id))
}

fn err_of(r: &Value) -> Option<String> {
    r.get("error").and_then(Value::as_str).map(str::to_string)
}

/// Make the bridge and the leg, name the file, grant, boot the leg. Returns its
/// `mcp_config_error`, as a note, when the write failed.
pub async fn ensure(h: &Host) -> Result<(), String> {
    if !exists(h, BRIDGE) {
        let r = h
            .operator("kernel", json!({"type": "create_agent", "parent_id": crate::env::HUMAN, "id": BRIDGE,
                "handler_module": "file_bridge.tools", "root": ".", "readonly": false,
                "allow": [FILE], "ingress_rule": "allow_all_ingress"}))
            .await
            .result;
        if let Some(e) = err_of(&r) {
            return Err(format!("the .mcp.json bridge `{BRIDGE}`: {e}"));
        }
    }
    crate::mcp::ensure_leg(h).await?;
    let leg = crate::mcp::LEG;
    let named = h.kernel.agents.get(&fantastic_kernel::AgentId::from(leg)).is_some_and(|a| {
        let m = &a.value().record().meta;
        m.get("file_bridge_id").is_some_and(|v| !v.is_null())
    });
    if !named {
        let r = h
            .operator("kernel", json!({"type": "update_agent", "id": leg, "file_bridge_id": BRIDGE, "mcp_config_file": FILE}))
            .await
            .result;
        if let Some(e) = err_of(&r) {
            return Err(format!("`{leg}`: file_bridge_id: {e}"));
        }
    }
    // both edges onto the bridge (the leg is a holder), then its boot writes
    crate::kern::reach::grant_all(h).await?;
    let _ = h.operator(leg, json!({"type": "boot"})).await;
    let r = h.operator(leg, json!({"type": "reflect", "tree": "none"})).await.result;
    match r.get("mcp_config_error").and_then(Value::as_str) {
        Some(e) => Err(format!("{FILE}: {e}")),
        None => Ok(()),
    }
}
