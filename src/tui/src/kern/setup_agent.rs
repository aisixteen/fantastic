//! THE CANVAS'S SET-UP CELL, host side: the `setup` agent.
//!
//! The canvas's set-up cell (`setup_view.ts`) does everything the terminal's
//! set-up page does — list the backends, write an AI agent's record, choose
//! where keys are kept and store a backend's key there, save / load / forget
//! saved set-ups, set the default — through the canvas's existing socket leg
//! (`canvas_ws`), with its OWN state (it does not drive the terminal's screen).
//!
//! NOT RUN BY DEFAULT. The canvas spawns this agent under the leg as the cell's
//! backend (the view/backend pair) and deletes it with the cell; `Host::boot`
//! deletes any left behind, so it is never hydrated (the cell then shows its
//! backend lost; ⟳ respawns it). While it exists it does, AS THE HUMAN, exactly
//! what the terminal's page does (`kern/setup`, `kern/keys`, `kern/saved`)
//! — the saved set-up store serves only the human, and the cell reaches it through
//! this agent, never directly.

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

use crate::env::ClientCfg;
use crate::kern::host::Host;

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "setup.tools";

/// Every verb the page calls (none is privileged: the leg's structural grants
/// reach its own child).
pub const VERBS: [&str; 13] = [
    "backends",
    "keys",
    "ai_settings",
    "configure_ai",
    "write_ai",
    "key_mode",
    "set_key_mode",
    "store_key",
    "saved",
    "save",
    "forget",
    "prefer",
    "reflect",
];

/// The bundle.
#[derive(Default)]
pub struct SetupBundle;

fn fail(e: impl Into<String>) -> Value {
    json!({"error": e.into(), "reason": "bad_arg"})
}

fn s(p: &Value, k: &str) -> String {
    p.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

impl SetupBundle {
    fn host(kernel: &Arc<Kernel>) -> Option<Host> {
        let root = kernel.root()?.id.0.clone();
        Some(Host { kernel: Arc::clone(kernel), cfg: ClientCfg::here(), root })
    }

    async fn run(host: &Host, verb: &str, p: &Value) -> Value {
        match verb {
            "backends" => json!({"backends": crate::kern::setup::backends(host).iter().map(|b| json!({
                "module": b.module,
                "key": b.key,
                "fields": b.fields.iter().map(|f| json!({
                    "name": f.name, "type": f.ty, "required": f.required, "enum": f.one_of,
                    "default": f.default, "hint": f.hint,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>()}),
            // the keys stored so far (ref + host, never a value) and where they are kept
            "keys" => match (crate::kern::keys::stored(), crate::kern::keys::mode()) {
                (Ok(refs), Ok((m, w))) => json!({
                    "keys": refs.iter().map(|(r, h)| json!({"ref": r, "host": h})).collect::<Vec<_>>(),
                    "mode": m.map(|m| m.name()),
                    "where": w,
                }),
                (Err(e), _) | (_, Err(e)) => fail(e),
            },
            // an AI agent's settings, as the wizard shows them: its handler and
            // every schema field's current value
            "ai_settings" => match ai_record(host, &s(p, "id")) {
                Ok((handler, meta)) => {
                    let fields: serde_json::Map<String, Value> = crate::kern::setup::backends(host)
                        .into_iter()
                        .find(|b| b.module == handler)
                        .map(|b| b.fields.iter().filter_map(|f| meta.get(&f.name).map(|v| (f.name.clone(), v.clone()))).collect())
                        .unwrap_or_default();
                    json!({"id": s(p, "id"), "handler": handler, "fields": fields, "saved": meta.get("saved")})
                }
                Err(e) => fail(format!("ai_settings: {e}")),
            },
            // change a live AI agent's fields (same backend): live on its next
            // turn — the backend reads its record per call. The key for its
            // endpoint, when one is stored, is named and granted.
            "configure_ai" => {
                let id = s(p, "id");
                let (handler, meta) = match ai_record(host, &id) {
                    Ok(r) => r,
                    Err(e) => return fail(format!("configure_ai: {e}")),
                };
                let mut upd = match typed_fields(host, &handler, p) {
                    Ok(f) => f,
                    Err(e) => return fail(format!("configure_ai: {e}")),
                };
                let endpoint = upd.get("endpoint").or_else(|| meta.get("endpoint")).and_then(Value::as_str).unwrap_or("").to_string();
                if crate::kern::keys::keyed(host, &handler).is_some() {
                    if let Ok(r) = crate::kern::keys::ref_for(&handler, &endpoint) {
                        if crate::kern::keys::has(host, &r) {
                            upd.insert("credentials".into(), json!({"store": crate::kern::keys::STORE, "ref": r}));
                            let mut cc = meta.get("can_call").and_then(Value::as_object).cloned().unwrap_or_default();
                            cc.insert(crate::kern::keys::STORE.into(), json!(["get"]));
                            upd.insert("can_call".into(), Value::Object(cc));
                            if let Err(e) = crate::kern::keys::wire(host, &id).await {
                                return fail(format!("configure_ai: {e}"));
                            }
                        }
                    }
                }
                let mut payload = json!({"type": "update_agent", "id": id});
                payload.as_object_mut().expect("an object").extend(upd);
                let r = host.act("kernel", payload).await.result;
                match r.get("error").and_then(Value::as_str) {
                    Some(e) => fail(format!("configure_ai: {e}")),
                    None => json!({"configured": id, "note": "live on its next turn"}),
                }
            }
            "write_ai" => {
                let handler = s(p, "handler");
                let fields = match typed_fields(host, &handler, p) {
                    Ok(f) => f,
                    Err(e) => return fail(format!("write_ai: {e}")),
                };
                let rec = crate::kern::setup::Record {
                    id: s(p, "id"),
                    handler,
                    fields,
                    may_call: p
                        .get("may_call")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default(),
                };
                match crate::kern::setup::write(host, &rec).await {
                    Ok(path) => json!({"written": path, "id": rec.id}),
                    Err(e) => fail(e),
                }
            }
            "key_mode" => match crate::kern::keys::mode() {
                Ok((m, w)) => json!({"mode": m.map(|m| m.name()), "where": w}),
                Err(e) => fail(e),
            },
            "set_key_mode" => match fantastic_user_home_store::secrets::Mode::parse(&s(p, "mode")) {
                Some(m) => match crate::kern::keys::set_mode(host, m).await {
                    Ok(w) => json!({"mode": m.name(), "where": w}),
                    Err(e) => fail(e),
                },
                None => fail(format!("set_key_mode: mode {:?} — plain or secure", s(p, "mode"))),
            },
            // The page's call carries the value unmarked (the page kernel has no
            // secret marks yet): its rows hold it. The onward `set` is marked.
            "store_key" => {
                let (module, endpoint) = (s(p, "module"), s(p, "endpoint"));
                let secret = p.get("secret").and_then(Value::as_str).unwrap_or("");
                if module.is_empty() || endpoint.is_empty() || secret.is_empty() {
                    return fail("store_key: module, endpoint and secret are required (a key is bound to its endpoint's host)");
                }
                match crate::kern::keys::store(host, &module, &endpoint, secret).await {
                    Ok(r) => json!({"stored": r, "note": "live on the next call — no restart"}),
                    Err(e) => fail(e),
                }
            }
            "saved" => match crate::kern::saved::shelf(host).await {
                Ok(sh) => json!({
                    "saved": sh.saved.iter().map(|t| json!({"name": t.name, "record": t.record})).collect::<Vec<_>>(),
                    "preferred": sh.preferred,
                    "dir": sh.dir,
                }),
                Err(e) => fail(e),
            },
            "save" => {
                let name = s(p, "name");
                let handler = s(p, "handler");
                let mut record = match typed_fields(host, &handler, p) {
                    Ok(f) => f,
                    Err(e) => return fail(format!("save: {e}")),
                };
                // the backend + every field set — never an id, a key or grants;
                // the stored key for its endpoint is NAMED (a ref, never the value):
                // a backend that requires one refuses a saved set-up without it
                if let Err(e) = crate::kern::keys::attach(host, &handler, &mut record) {
                    return fail(format!("save: {e}"));
                }
                record.insert("handler_module".into(), json!(handler));
                let record = Value::Object(record);
                match crate::kern::saved::save(host, &name, record).await {
                    Ok(path) => json!({"saved": name, "path": path}),
                    Err(e) => fail(e),
                }
            }
            "forget" => {
                let r = host
                    .act(crate::kern::saved::HOME_ID, json!({"type": "forget_record", "name": s(p, "name")}))
                    .await
                    .result;
                if r.get("error").is_some() {
                    r
                } else {
                    json!({"forgotten": s(p, "name")})
                }
            }
            "prefer" => {
                let name = p.get("name").and_then(Value::as_str).map(str::to_string);
                match crate::kern::saved::prefer(host, name.as_deref()).await {
                    Ok(pr) => json!({"preferred": pr}),
                    Err(e) => fail(e),
                }
            }
            _ => json!({"error": format!("setup: unknown type {verb:?}"), "reason": "unhandled", "verb": verb}),
        }
    }
}

#[async_trait]
impl Bundle for SetupBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    /// HOW TO CALL it — served by the kernel's `docs` verb, never on a reflect.
    fn verb_docs(&self) -> Value {
        Value::Object(VERBS.iter().map(|v| ((*v).to_string(), json!(""))).collect())
    }

    async fn handle(&self, _agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if verb == "boot" {
            return Ok(None);
        }
        if verb == "reflect" {
            return Ok(Some(json!({
                "handler_module": HANDLER_MODULE,
                "sentence": "The canvas's AI set-up cell, host side: what the terminal's set-up page does, as the human. Exists only while the cell is open.",
            })));
        }
        let Some(host) = Self::host(kernel) else {
            return Ok(Some(fail("setup: the kernel has no root")));
        };
        Ok(Some(Self::run(&host, verb, payload).await))
    }
}

/// The page's field TEXTS (`fields: {name: text}`), typed by `handler`'s schema.
fn typed_fields(host: &Host, handler: &str, p: &Value) -> Result<serde_json::Map<String, Value>, String> {
    crate::kern::setup::typed(host, handler, &p.get("fields").and_then(Value::as_object).cloned().unwrap_or_default())
}

/// An AI agent's handler and record fields — refused for any other agent.
fn ai_record(host: &Host, id: &str) -> Result<(String, serde_json::Map<String, Value>), String> {
    let a = host.kernel.agents.get(&AgentId::from(id)).ok_or_else(|| format!("no agent {id:?}"))?;
    let rec = a.value().record();
    let handler = rec.handler_module.clone().unwrap_or_default();
    if !crate::kern::setup::backends(host).iter().any(|b| b.module == handler) {
        return Err(format!("{id:?} is not an AI agent ({handler})"));
    }
    Ok((handler, rec.meta.clone()))
}

/// Delete every `setup` agent a previous run left behind: it lives only while a
/// page is open, and is never hydrated.
pub async fn sweep(host: &Host) {
    let stale: Vec<String> = host
        .kernel
        .agents
        .iter()
        .filter(|e| e.value().record().handler_module.as_deref() == Some(HANDLER_MODULE))
        .map(|e| e.key().0.clone())
        .collect();
    for id in stale {
        let _ = host.operator("kernel", json!({"type": "delete_agent", "id": id})).await;
    }
}
