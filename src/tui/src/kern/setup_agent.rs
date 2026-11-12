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
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

use crate::env::ClientCfg;
use crate::kern::host::Host;

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "setup.tools";

pub mod record;
pub mod verbs;

/// What the bundle is for: its one short man text; how to call it is rendered from its
/// declared verbs (`docs`, `reflect`).
const MAN: &str = "The canvas's AI set-up cell, host side: what the terminal's set-up page does (list the backends, write an AI agent, choose where keys are kept and store a key, save and forget set-ups, set the default), as the human. It exists only while the cell is open.";

/// The bundle.
#[derive(Default)]
pub struct SetupBundle;

fn fail(e: impl Into<String>) -> Value {
    json!({"error": e.into(), "reason": "bad_arg"})
}

impl SetupBundle {
    fn host(kernel: &Arc<Kernel>) -> Option<Host> {
        let root = kernel.root()?.id.0.clone();
        Some(Host { kernel: Arc::clone(kernel), cfg: ClientCfg::here(), root })
    }

    async fn run(host: &Host, call: verbs::Call) -> Value {
        use verbs::Call;
        match call {
            Call::Boot(_) | Call::Reflect(_) => unreachable!("answered before the host is built"),
            Call::Backends(_) => json!({"backends": crate::kern::setup::backends(host).iter().map(|b| json!({
                "module": b.module,
                "key": b.key,
                "fields": b.fields.iter().map(|f| json!({
                    "name": f.name, "type": f.ty, "required": f.required, "enum": f.one_of,
                    "default": f.default, "hint": f.hint,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>()}),
            // the keys stored so far (ref + host, never a value) and where they are kept
            Call::Keys(_) => match (crate::kern::keys::stored(), crate::kern::keys::mode()) {
                (Ok(refs), Ok((m, w))) => json!({
                    "keys": refs.iter().map(|(r, h)| json!({"ref": r, "host": h})).collect::<Vec<_>>(),
                    "mode": m.map(|m| m.name()),
                    "where": w,
                }),
                (Err(e), _) | (_, Err(e)) => fail(e),
            },
            // an AI agent's settings, as the wizard shows them: its handler and
            // every schema field's current value
            Call::AiSettings(a) => match ai_record(host, &a.id) {
                Ok((handler, meta)) => {
                    let fields: serde_json::Map<String, Value> = crate::kern::setup::backends(host)
                        .into_iter()
                        .find(|b| b.module == handler)
                        .map(|b| b.fields.iter().filter_map(|f| meta.get(&f.name).map(|v| (f.name.clone(), v.clone()))).collect())
                        .unwrap_or_default();
                    json!({"id": a.id, "handler": handler, "fields": fields, "saved": meta.get("saved")})
                }
                Err(e) => fail(format!("ai_settings: {e}")),
            },
            // change a live AI agent's fields (same backend): live on its next
            // turn — the backend reads its record per call. The key for its
            // endpoint, when one is stored, is named and granted.
            Call::ConfigureAi(a) => {
                let id = a.id;
                let (handler, meta) = match ai_record(host, &id) {
                    Ok(r) => r,
                    Err(e) => return fail(format!("configure_ai: {e}")),
                };
                let mut upd = match crate::kern::setup::declared_fields(host, "configure_ai", &handler, a.fields.unwrap_or_default()) {
                    Ok(m) => m,
                    Err(refusal) => return refusal,
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
                // declared names only, then the verb and the target — a field can never replace either
                upd.insert("type".into(), json!("update_agent"));
                upd.insert("id".into(), json!(id));
                let payload = Value::Object(upd);
                let r = host.act("kernel", payload).await.result;
                match r.get("error").and_then(Value::as_str) {
                    Some(e) => fail(format!("configure_ai: {e}")),
                    None => json!({"configured": id, "note": "live on its next turn"}),
                }
            }
            Call::WriteAi(a) => {
                let fields = match crate::kern::setup::declared_fields(host, "write_ai", &a.handler, a.fields.unwrap_or_default()) {
                    Ok(m) => m,
                    Err(refusal) => return refusal,
                };
                let rec = crate::kern::setup::Record {
                    id: a.id,
                    handler: a.handler,
                    fields,
                    may_call: a.may_call.unwrap_or_default(),
                };
                match crate::kern::setup::write(host, &rec).await {
                    Ok(path) => json!({"written": path, "id": rec.id}),
                    Err(e) => fail(e),
                }
            }
            Call::KeyMode(_) => match crate::kern::keys::mode() {
                Ok((m, w)) => json!({"mode": m.map(|m| m.name()), "where": w}),
                Err(e) => fail(e),
            },
            Call::SetKeyMode(a) => {
                let m = match a.mode {
                    verbs::Mode::Plain => fantastic_user_home_store::secrets::Mode::Plain,
                    verbs::Mode::Secure => fantastic_user_home_store::secrets::Mode::Secure,
                };
                match crate::kern::keys::set_mode(host, m).await {
                    Ok(w) => json!({"mode": m.name(), "where": w}),
                    Err(e) => fail(e),
                }
            }
            // The page's call carries the value unmarked (the page kernel has no
            // secret marks yet): its rows hold it. The onward `set` is marked.
            Call::StoreKey(a) => match crate::kern::keys::store(host, &a.module, &a.endpoint, &a.secret).await {
                Ok(r) => json!({"stored": r, "note": "live on the next call — no restart"}),
                Err(e) => fail(e),
            },
            Call::Saved(_) => match crate::kern::saved::shelf(host).await {
                Ok(sh) => json!({
                    "saved": sh.saved.iter().map(|t| json!({"name": t.name, "record": t.record})).collect::<Vec<_>>(),
                    "preferred": sh.preferred,
                    "dir": sh.dir,
                }),
                Err(e) => fail(e),
            },
            Call::Save(a) => {
                let mut record = match crate::kern::setup::declared_fields(host, "save", &a.handler, a.fields.unwrap_or_default()) {
                    Ok(m) => m,
                    Err(refusal) => return refusal,
                };
                // the backend + every field set — never an id, a key or grants;
                // the stored key for its endpoint is NAMED (a ref, never the value):
                // a backend that requires one refuses a saved set-up without it
                if let Err(e) = crate::kern::keys::attach(host, &a.handler, &mut record) {
                    return fail(format!("save: {e}"));
                }
                record.insert("handler_module".into(), json!(a.handler));
                let record = Value::Object(record);
                match crate::kern::saved::save(host, &a.name, record).await {
                    Ok(path) => json!({"saved": a.name, "path": path}),
                    Err(e) => fail(e),
                }
            }
            Call::Forget(a) => {
                let r = host
                    .act(crate::kern::saved::HOME_ID, json!({"type": "forget_record", "name": a.name}))
                    .await
                    .result;
                if r.get("error").is_some() {
                    r
                } else {
                    json!({"forgotten": a.name})
                }
            }
            Call::Prefer(a) => match crate::kern::saved::prefer(host, a.name.as_deref()).await {
                Ok(pr) => json!({"preferred": pr}),
                Err(e) => fail(e),
            },
        }
    }
}

#[async_trait]
impl Handler for SetupBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    async fn handle(&self, _agent_id: &AgentId, call: Self::Call, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        match call {
            verbs::Call::Boot(_) => return Ok(None),
            verbs::Call::Reflect(_) => {
                return Ok(Some(json!({
                    "handler_module": HANDLER_MODULE,
                    "sentence": "The canvas's AI set-up cell, host side: what the terminal's set-up page does, as the human. Exists only while the cell is open.",
                })))
            }
            _ => {}
        }
        let Some(host) = Self::host(kernel) else {
            return Ok(Some(fail("setup: the kernel has no root")));
        };
        Ok(Some(Self::run(&host, call).await))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One call through the agent's entry, on a kernel with no root: the declared verbs
    /// check it before any handler runs.
    async fn ask(payload: Value) -> Value {
        let kernel = Arc::new(Kernel::new());
        kernel.register(fantastic_kernel::Agent::new(
            AgentId::from("setup"),
            None,
            None,
            Default::default(),
            std::path::PathBuf::new(),
            false,
        ));
        fantastic_kernel::Bundle::handle_binary(&SetupBundle, &AgentId::from("setup"), payload, None, &kernel)
            .await
            .unwrap()
            .0
            .unwrap_or(Value::Null)
    }

    fn why(r: &Value) -> (Option<&str>, Option<&str>) {
        (r["reason"].as_str(), r["arg"].as_str())
    }

    #[tokio::test]
    async fn every_verb_is_checked_by_its_declaration() {
        let r = ask(json!({"type": "zzz_nope"})).await;
        assert_eq!(r["reason"], "unhandled", "{r}");
        for v in verbs::VERBS.iter().map(|v| v.name).chain(["reflect"]) {
            assert!(r["error"].as_str().unwrap().contains(v), "{v} in {r}");
        }
        for v in verbs::VERBS {
            let r = ask(json!({"type": v.name, "zzz_unknown": 1})).await;
            assert_eq!(why(&r), (Some("bad_arg"), Some("zzz_unknown")), "{}: {r}", v.name);
            let required: Vec<String> =
                fantastic_kernel::verbs::args(v.schema()).into_iter().filter(|a| a.required).map(|a| a.name).collect();
            if !required.is_empty() {
                let r = ask(json!({"type": v.name})).await;
                assert_eq!(r["reason"], "missing_arg", "{}: {r}", v.name);
                assert!(required.iter().any(|n| r["arg"] == json!(n)), "{}: {r}", v.name);
            }
        }
        // wrongly typed, never coerced
        for (p, arg) in [
            (json!({"type": "ai_settings", "id": 3}), "id"),
            (json!({"type": "configure_ai", "id": "a", "fields": ["x"]}), "fields"),
            (json!({"type": "write_ai", "id": "a", "handler": "vllm_backend.tools", "may_call": "x"}), "may_call"),
            (json!({"type": "write_ai", "id": "  ", "handler": "vllm_backend.tools"}), "id"),
            (json!({"type": "set_key_mode", "mode": "plaintext"}), "mode"),
            (json!({"type": "forget", "name": 3}), "name"),
            (json!({"type": "prefer", "name": ""}), "name"),
        ] {
            let r = ask(p.clone()).await;
            assert_eq!(r["reason"], "bad_arg", "{p}: {r}");
            assert_eq!(r["arg"], json!(arg), "{p}: {r}");
        }
        // an empty required text is as good as none
        let r = ask(json!({"type": "store_key", "module": "m", "endpoint": "e", "secret": ""})).await;
        assert_eq!(why(&r), (Some("missing_arg"), Some("secret")), "{r}");
        // the one answer needing no host: reflect
        let r = ask(json!({"type": "reflect"})).await;
        assert_eq!(r["handler_module"], HANDLER_MODULE, "{r}");
    }
}
