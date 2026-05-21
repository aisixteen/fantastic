//! Spatial UI host as an agent.
//!
//! The canvas is a web page that renders OTHER agents as positioned
//! iframes. Membership is **structural** — agents added to the canvas
//! become its children, so cascade-delete tears down the subtree
//! cleanly when the canvas dies.
//!
//! Layout (x, y, width, height) lives on each member's record.
//! Drag/resize in the browser sends `update_agent` against the
//! canvas backend; the substrate emits state events that the UI
//! mirrors.
//!
//! ## Verbs
//!
//! - `reflect` → `{id, sentence, member_count, viewport_default}`
//! - `boot`    → no-op (canvas is browser-driven).
//! - `list_members` → `{members: [id, …]}` — direct children.
//! - `add_agent` args `{handler_module}` or `{agent_id}` →
//!   `{ok, members, member_id, already?}`. Spawns a new member as a
//!   child OR re-parents an existing one. Refused if the resulting
//!   member doesn't answer `get_webapp` (or `get_gl_view`, when GL
//!   views land in a later phase).
//! - `remove_agent` args `{agent_id}` → `{removed, members}`.
//!   Cascade-deletes the member + its subtree.
//! - `discover` args `{x, y, w, h}` → `{agents: [id, …]}` — spatial
//!   intersection over member records.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "canvas_backend.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// The canvas-backend bundle.
pub struct CanvasBackendBundle;

#[async_trait]
impl Bundle for CanvasBackendBundle {
    fn name(&self) -> &str {
        "canvas_backend"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, kernel),
            "boot" | "shutdown" => Value::Null,
            "list_members" => list_members_reply(agent_id, kernel),
            "add_agent" => add_agent_reply(agent_id, payload, kernel).await,
            "remove_agent" => remove_agent_reply(agent_id, payload, kernel).await,
            "discover" => discover_reply(agent_id, payload, kernel),
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

fn reflect_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let canvas = kernel.agents.get(agent_id).map(|e| Arc::clone(&e));
    let member_count = canvas.as_ref().map(|c| c.child_count()).unwrap_or(0);
    json!({
        "id": agent_id.as_str(),
        "sentence": "Spatial UI host. Members are structural children; cascade delete owns the subtree.",
        "member_count": member_count,
        "viewport_default": {"width": 320, "height": 220},
        "verbs": {
            "reflect": "Identity + member_count. No args.",
            "boot": "No-op.",
            "list_members": "{members:[id,...]} — direct children.",
            "add_agent": "args: handler_module:str | agent_id:str. Spawn or re-parent a member.",
            "remove_agent": "args: agent_id:str. Cascade-delete a member.",
            "discover": "args: x:float, y:float, w:float, h:float. Spatial intersection.",
        }
    })
}

fn list_members_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let canvas = kernel.agents.get(agent_id).map(|e| Arc::clone(&e));
    let members: Vec<String> = canvas
        .map(|c| c.child_ids().into_iter().map(|i| i.0).collect())
        .unwrap_or_default();
    json!({"members": members})
}

async fn add_agent_reply(canvas_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let handler_module = payload.get("handler_module").and_then(Value::as_str);
    let existing_id = payload.get("agent_id").and_then(Value::as_str);
    let canvas = match kernel.agents.get(canvas_id).map(|e| Arc::clone(&e)) {
        Some(c) => c,
        None => return json!({"error": format!("no canvas {canvas_id}")}),
    };

    // Path A: spawn a fresh member as a child of the canvas.
    let new_member_id: AgentId = if let Some(hm) = handler_module {
        // Build a create_agent payload that flattens every key from the
        // original (so x/y/width/height land on the new record) except
        // the framing fields.
        let mut create_payload = serde_json::Map::new();
        create_payload.insert("type".to_string(), json!("create_agent"));
        create_payload.insert("handler_module".to_string(), json!(hm));
        if let Some(obj) = payload.as_object() {
            for (k, v) in obj {
                if matches!(k.as_str(), "type" | "handler_module" | "agent_id") {
                    continue;
                }
                create_payload.insert(k.clone(), v.clone());
            }
        }
        let reply = kernel.send(canvas_id, Value::Object(create_payload)).await;
        if let Some(err) = reply.get("error").and_then(Value::as_str) {
            return json!({"error": format!("add_agent: create failed: {err}")});
        }
        let id = match reply.get("id").and_then(Value::as_str) {
            Some(s) => AgentId::from(s),
            None => return json!({"error": "add_agent: create returned no id"}),
        };
        id
    } else if let Some(id) = existing_id {
        // Path B: re-parent an existing agent under this canvas.
        // Substrate doesn't expose re-parenting as a system verb yet
        // (Phase 1 scope), so for now we refuse with a clear message.
        return json!({
            "error": format!(
                "add_agent: re-parenting existing agent {id} not yet supported (Phase 1)",
            ),
        });
    } else {
        return json!({"error": "add_agent: requires handler_module or agent_id"});
    };

    // Verify the new member answers either renderable verb. Probe
    // both `get_webapp` (DOM iframe contract: `{url, ...}`) and
    // `get_gl_view` (WebGL contract: `{source, ...}`)
    // — Python parity (`canvas_backend/tools.py:119-129`). gl_agent
    // + telemetry_pane only answer the GL verb; without this probe
    // they can't sit on a Rust canvas.
    let wa = kernel
        .send(&new_member_id, json!({"type": "get_webapp"}))
        .await;
    let has_dom = wa.is_object() && wa.get("error").is_none() && wa.get("url").is_some();
    let gl = kernel
        .send(&new_member_id, json!({"type": "get_gl_view"}))
        .await;
    // Python parity (`canvas_backend/tools.py:123`): `has_gl` checks
    // `gl.get("source")`. Match exactly so cross-runtime workdirs
    // route the same.
    let has_gl = gl.is_object() && gl.get("error").is_none() && gl.get("source").is_some();
    if !(has_dom || has_gl) {
        // Cascade-delete the just-spawned member — canvas-eligible
        // requires a UI verb.
        kernel
            .send(
                canvas_id,
                json!({"type": "delete_agent", "id": new_member_id.0}),
            )
            .await;
        return json!({
            "error": format!(
                "add_agent: '{}' answers neither get_webapp nor get_gl_view; nothing to render",
                new_member_id,
            ),
        });
    }

    let members: Vec<String> = canvas.child_ids().into_iter().map(|i| i.0).collect();
    // Emit members_updated on the canvas inbox so watchers refresh.
    kernel
        .emit(
            canvas_id,
            json!({"type": "members_updated", "members": members.clone()}),
        )
        .await;
    json!({
        "ok": true,
        "member_id": new_member_id.0,
        "members": members,
    })
}

async fn remove_agent_reply(canvas_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let target_id = match payload.get("agent_id").and_then(Value::as_str) {
        Some(s) => AgentId::from(s),
        None => return json!({"error": "remove_agent requires agent_id"}),
    };
    let canvas = match kernel.agents.get(canvas_id).map(|e| Arc::clone(&e)) {
        Some(c) => c,
        None => return json!({"error": format!("no canvas {canvas_id}")}),
    };
    if !canvas.has_child(&target_id) {
        // Idempotent — Python parity (`canvas_backend/tools.py:150-151`).
        // Caller can retry remove_agent safely; the second call is a
        // no-op rather than an error.
        let members: Vec<String> = canvas.child_ids().into_iter().map(|i| i.0).collect();
        return json!({"removed": false, "members": members});
    }
    let del = kernel
        .send(
            canvas_id,
            json!({"type": "delete_agent", "id": target_id.0}),
        )
        .await;
    if del.get("error").is_some() && del.get("locked").is_none() {
        return json!({
            "error": format!(
                "remove_agent: cascade delete failed: {}",
                del.get("error").and_then(Value::as_str).unwrap_or("?"),
            ),
        });
    }
    let members: Vec<String> = canvas.child_ids().into_iter().map(|i| i.0).collect();
    kernel
        .emit(
            canvas_id,
            json!({"type": "members_updated", "members": members.clone()}),
        )
        .await;
    json!({"removed": true, "id": target_id.0, "members": members})
}

fn discover_reply(canvas_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let canvas = match kernel.agents.get(canvas_id).map(|e| Arc::clone(&e)) {
        Some(c) => c,
        None => return json!({"error": format!("no canvas {canvas_id}")}),
    };
    let bx = payload.get("x").and_then(Value::as_f64).unwrap_or(0.0);
    let by = payload.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    let bw = payload.get("w").and_then(Value::as_f64).unwrap_or(0.0);
    let bh = payload.get("h").and_then(Value::as_f64).unwrap_or(0.0);
    // Python parity (`canvas_backend/tools.py:170-171`) — a zero/
    // negative box is a caller bug, not "no hits". Returning an
    // explicit error matches the Python selftest's Test 2 assertion.
    if bw <= 0.0 || bh <= 0.0 {
        return json!({"error": "discover: w and h required and > 0"});
    }
    let mut hits: Vec<String> = Vec::new();
    for cid in canvas.child_ids() {
        let Some(child) = kernel.agents.get(&cid).map(|e| Arc::clone(&e)) else {
            continue;
        };
        let meta = child.meta.read().expect("meta poisoned");
        let (ax, ay, aw, ah) = (
            meta.get("x").and_then(Value::as_f64).unwrap_or(0.0),
            meta.get("y").and_then(Value::as_f64).unwrap_or(0.0),
            meta.get("width").and_then(Value::as_f64).unwrap_or(320.0),
            meta.get("height").and_then(Value::as_f64).unwrap_or(220.0),
        );
        if intersects((ax, ay, aw, ah), (bx, by, bw, bh)) {
            hits.push(cid.0);
        }
    }
    json!({"agents": hits})
}

fn intersects(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;
    !(ax + aw < bx || bx + bw < ax || ay + ah < by || by + bh < ay)
}

#[cfg(test)]
mod tests;
