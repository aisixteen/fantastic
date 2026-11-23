//! THE PROJECT'S MCP CONFIG — `.mcp.json`, in the folder of its own bridge
//! (`<state>/agents/user/agents/web/agents/mcp/agents/mcp_config/`), kept by the
//! `mcp` leg itself (owner 2026-10-08; inside the state folder: a project is never
//! polluted). A coding CLI points at it (`claude --mcp-config <that file>`;
//! `<state>/readme.md` names the path) to find THIS kernel:
//! `mcpServers.fantastic = {command: <this binary>, args: [mcp]}`. Only the `mcp`
//! agent writes that file, through its own bridge (`fantastic-web-mcp`: on its
//! boot, read → set its entry → write if changed, every other server kept).
//!
//! Both records are the blueprint's: [`BRIDGE`], a WRITABLE file_bridge rooted at
//! `@self` exposing only [`FILE`], under the leg, which names `file_bridge_id` +
//! `mcp_config_file` and reaches its bridge by position. The leg's boot writes the
//! file; [`check`] reports its `mcp_config_error`, if any.

use serde_json::{json, Value};

use crate::kern::host::Host;

/// The bridge the leg writes through.
pub const BRIDGE: &str = "mcp_config";
/// The file, in the bridge's own folder.
pub const FILE: &str = ".mcp.json";

/// The leg's `mcp_config_error` after its boot, as an `Err`, when the write failed.
pub async fn check(h: &Host) -> Result<(), String> {
    let leg = crate::mcp::LEG;
    if !h.kernel.exists(leg) || !h.kernel.exists(BRIDGE) {
        return Err(format!("`{leg}` or its bridge `{BRIDGE}` is not in the tree"));
    }
    let r = h.operator(leg, json!({"type": "reflect", "tree": "none"})).await.result;
    match r.get("mcp_config_error").and_then(Value::as_str) {
        Some(e) => Err(format!("{FILE}: {e}")),
        None => Ok(()),
    }
}
