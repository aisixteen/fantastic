//! `agentic_resources` — a generic registry of agent ROLES.
//!
//! A role card is one file (`cards`); the registry lists them (`roles`, `role`),
//! creates an agent from one (`spawn`), reports who it spawned (`fleet`) and caps
//! how many run (`set_limit`, `get_count`). Not code-oriented: a role is any kind
//! of agent. OFF BY DEFAULT: a host registers the bundle only when
//! `FANTASTIC_AGENTIC_RESOURCES` is truthy ([`register_if_enabled`]), and an
//! instance whose record has no `role_dirs` refuses every verb `disabled`.
//!
//! Config is the registry agent's own record: `role_dirs` (folders of cards),
//! `tiers` (`{tier: backend fields}` — the card names a tier KEY, never a model),
//! `limits` (`{total?, per_role?: {role: n}}`, kept by `set_limit`).
//!
//! NOTHING is counted in memory. Every spawned agent carries `spawned_by` (this
//! registry) and `role`, so `fleet` and `get_count` are read off the records: a
//! delete lowers the count and a restart rebuilds it. `running` counts
//! EVERY spawned agent that still exists, so a slot frees only on
//! `delete_agent`. The task tree is not kept here: that is `task_registry`.
//!
//! NO CLEANUP: `report` only messages the parent. Nothing is retired or deleted;
//! `delete_agent` is the only way out.
//!
//! `spawn` may branch `from` a fork point of an existing agent (`from:{agent, at}`):
//! the registry forks it (ai_core `fork {at}`) and applies the role over the copy.
//!
//! Verbs (mirror Python verb-for-verb): `reflect`, `roles`, `role`, `spawn`,
//! `report`, `fleet`, `set_limit`, `get_count`.

#![deny(missing_docs)]

pub mod cards;

use async_trait::async_trait;
use cards::{Card, REGISTRY_TOKEN};
use fantastic_bundle::py_repr;
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::refusal::{self, bad, missing};
use fantastic_kernel::{current_origin, Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "agentic_resources.tools";

/// The environment variable that turns the bundle on in a host.
pub const ENABLE_ENV: &str = "FANTASTIC_AGENTIC_RESOURCES";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// The states a `report` may carry.
pub const STATES: [&str; 6] = ["waiting", "running", "blocked", "done", "failed", "paused"];

/// What a parent holds over every agent it spawns.
const PARENT_VERBS: [&str; 3] = ["pause", "resume", "stop"];

/// Is an env value on? `1`, `true`, `yes`, `on` (any case).
#[must_use]
pub fn enabled_value(v: Option<&str>) -> bool {
    v.is_some_and(|s| {
        matches!(
            s.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Register the bundle only when `value` (the env var) turns it on. Returns
/// whether it registered. Off: no handler, so nothing changes for the host.
pub fn register_if_enabled(reg: &mut BundleRegistry, value: Option<&str>) -> bool {
    if enabled_value(value) {
        reg.register(HANDLER_MODULE, AgenticResourcesBundle);
        true
    } else {
        false
    }
}

/// The agentic_resources bundle.
pub struct AgenticResourcesBundle;

fn verb_docs_json() -> Value {
    json!({
        "reflect": "Identity, whether the registry is enabled, and the role names. No args.",
        "roles": "No args. Returns {roles:[{name, purpose}], errors:[...]} — every role card the config's folders hold, plus the shipped project_lead. Cards are re-read on each call.",
        "role": "args: name:str (req). Returns the full card {name, purpose, tier, grants, directive}; an unknown name is `not_found`.",
        "spawn": "args: role:str (req), task:str? (sent to the child as its first message; anything but a string is `bad_arg`), name:str?, limits:{total?, per_role?}? (part of YOUR quota handed to the child as its own subtree cap). Creates the agent from the card (directive as its system prompt, the tier's backend fields from config, the card's grants) as a CHILD OF THE CALLER, and gives the caller pause/resume/stop on it. Refused past a limit: {error:'limit', scope, role, running, max, reason}. Returns {id, role, tier}. from:{agent:str, at:int}? branches the new agent off a fork point of `agent` instead (ai_core `fork {at}`, same memory store, so it starts with that conversation up to `at`), then applies the card and limits; it sits BESIDE its source, not under you, and you still get pause/resume/stop on it; you must hold `fork` on `agent` or it is refused `not_allowed`; a refusal from fork (cuts_turn, cuts_fold, no_such_row, parent_mid_turn, ...) passes through unchanged and nothing is left behind. Then the reply also has from:{agent, at} and copied. The card's grants on tools are opened on both ends (the child's own grant and the tool's callable_by, all or nothing): a card may only grant what the spawner itself holds, or the spawn is refused `not_allowed`, and a grant that cannot be written undoes the whole spawn (`grant_failed`).",
        "report": "args: state:waiting|running|blocked|done|failed|paused (req), result:any?. A spawned agent tells its parent: the parent gets a message {type:'report', from, state, result}. Nothing is stored on the record (the task tree is `task_registry`'s). Refused `not_spawned` for any other caller. Nothing is cleaned up on done. Returns {ok:true}.",
        "fleet": "No args. Returns {roles:{<role>:[{id}]}} — the agents this registry spawned, per role, read from the records (survives restart).",
        "set_limit": "args: role:str? (omitted = all agents in total), max:int|null (req; null clears). Returns {ok:true, role, max}.",
        "get_count": "args: role:str? (omitted = in total). Returns {running, max} — running counts every agent this registry spawned that still exists (a slot frees on delete_agent); max is null when uncapped.",
    })
}

// ── record helpers ──────────────────────────────────────────────────

fn meta_of(a: &Agent) -> Map<String, Value> {
    a.meta.read().expect("meta poisoned").clone()
}

fn agent_of(kernel: &Kernel, id: &AgentId) -> Option<Arc<Agent>> {
    kernel.agents.get(id).map(|e| Arc::clone(&e))
}

/// The registry's `role_dirs`, or `None` when the registry is not configured.
fn role_dirs(meta: &Map<String, Value>) -> Option<Vec<String>> {
    let arr = meta.get("role_dirs")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
    )
}

fn disabled() -> Value {
    json!({"error": "agentic_resources is disabled"})
}

/// A refusal of the registry's own: `{error:<code>, reason:<text>, ...}`.
fn fail(code: &str, reason: String) -> Value {
    json!({"error": code, "reason": reason})
}

/// One agent this registry spawned.
struct Spawned {
    id: AgentId,
    role: String,
    /// The lead that spawned it `from` a fork point (it sits beside its source, so
    /// the tree alone would not put it under that lead's quota).
    lead: Option<String>,
}

/// Is `s` in `lead`'s subtree: below it in the tree, or spawned by it `from` a fork?
fn below_lead(kernel: &Kernel, s: &Spawned, lead: &AgentId) -> bool {
    s.lead.as_deref() == Some(lead.as_str()) || ancestors(kernel, &s.id).contains(lead)
}

fn spawned(kernel: &Kernel, reg: &AgentId) -> Vec<Spawned> {
    let mut out: Vec<Spawned> = kernel
        .agents
        .iter()
        .filter_map(|e| {
            let m = meta_of(e.value());
            (m.get("spawned_by").and_then(Value::as_str) == Some(reg.as_str())).then(|| Spawned {
                id: e.value().id.clone(),
                role: m
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                lead: m.get("lead").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    out
}

/// Every id above `id`, nearest first.
fn ancestors(kernel: &Kernel, id: &AgentId) -> Vec<AgentId> {
    let mut out = Vec::new();
    let mut cur = agent_of(kernel, id).and_then(|a| a.parent_id.clone());
    while let Some(p) = cur {
        cur = agent_of(kernel, &p).and_then(|a| a.parent_id.clone());
        out.push(p);
    }
    out
}

fn cap(limits: &Value, role: Option<&str>) -> Option<u64> {
    match role {
        Some(r) => limits.get("per_role")?.get(r)?.as_u64(),
        None => limits.get("total")?.as_u64(),
    }
}

fn limit_refusal(scope: &str, role: &str, running: usize, max: u64, who: Option<&str>) -> Value {
    let what = match scope {
        "role" => format!("role {} is at its limit", py_repr(role)),
        "total" => "all roles together are at the limit".to_string(),
        "request" => format!(
            "the limits asked for exceed {}'s remaining quota",
            who.unwrap_or("the caller")
        ),
        _ => format!(
            "{}'s delegated quota is used up",
            who.unwrap_or("the caller")
        ),
    };
    let mut r = json!({
        "error": "limit", "scope": scope, "role": role,
        "running": running, "max": max,
        "reason": format!("{what} ({running} running, max {max})"),
    });
    if let Some(w) = who {
        r["lead"] = json!(w);
    }
    r
}

/// Add `verbs` to a grant map's `key` (a `"*"` stays `"*"`).
fn grant_verbs(map: &mut Map<String, Value>, key: &str, verbs: &Value) {
    let merged = match (map.get(key), verbs) {
        (Some(Value::String(s)), _) if s == "*" => json!("*"),
        (_, Value::String(s)) if s == "*" => json!("*"),
        (Some(Value::Array(a)), Value::Array(b)) => {
            let mut all = a.clone();
            for v in b {
                if !all.contains(v) {
                    all.push(v.clone());
                }
            }
            Value::Array(all)
        }
        _ => verbs.clone(),
    };
    map.insert(key.to_string(), merged);
}

fn cards_of(meta: &Map<String, Value>) -> (Vec<Card>, Vec<String>) {
    cards::load_dir(&role_dirs(meta).unwrap_or_default())
}

fn roles_list(cards: &[Card]) -> Value {
    Value::Array(
        cards
            .iter()
            .map(|c| json!({"name": c.name, "purpose": c.purpose}))
            .collect(),
    )
}

// ── the bundle ──────────────────────────────────────────────────────

#[async_trait]
impl Bundle for AgenticResourcesBundle {
    fn name(&self) -> &str {
        "agentic_resources"
    }

    fn verb_docs(&self) -> Value {
        verb_docs_json()
    }

    /// The `roles` doc carries the live list, so any agent with reach learns who is who.
    fn instance_verb_docs(&self, agent: &Agent) -> Option<Value> {
        let meta = meta_of(agent);
        role_dirs(&meta)?;
        let (cards, _) = cards_of(&meta);
        let list = cards
            .iter()
            .map(|c| format!("{}: {}", c.name, c.purpose))
            .collect::<Vec<_>>()
            .join("; ");
        let mut docs = verb_docs_json();
        let base = docs["roles"].as_str().unwrap_or("").to_string();
        docs["roles"] = json!(format!("{base} Roles now: {list}"));
        Some(docs)
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
        let raw = payload.get("type").unwrap_or(&Value::Null);
        let verb = raw.as_str().unwrap_or("");
        let Some(me) = agent_of(kernel, agent_id) else {
            return Ok(None);
        };
        let meta = meta_of(&me);
        let on = role_dirs(&meta).is_some();
        let reply = match verb {
            "boot" | "shutdown" => return Ok(None),
            "reflect" => {
                let names: Vec<String> = if on {
                    cards_of(&meta).0.into_iter().map(|c| c.name).collect()
                } else {
                    Vec::new()
                };
                json!({
                    "id": agent_id.as_str(),
                    "sentence": "A registry of agent roles: list them, spawn from a card, cap how many run.",
                    "enabled": on, "roles": names,
                })
            }
            _ if !on => disabled(),
            "roles" => {
                let (cards, errors) = cards_of(&meta);
                json!({"roles": roles_list(&cards), "errors": errors})
            }
            "role" => role_reply(&meta, payload),
            "spawn" => spawn_reply(agent_id, &meta, payload, kernel).await,
            "report" => report_reply(agent_id, payload, kernel).await,
            "fleet" => fleet_reply(agent_id, kernel),
            "set_limit" => set_limit_reply(agent_id, &meta, payload, kernel),
            "get_count" => get_count_reply(agent_id, &meta, payload, kernel),
            _ => {
                let mut r = refusal::refusal(
                    refusal::UNHANDLED,
                    format!(
                        "agentic_resources: unknown type {}",
                        fantastic_bundle::py_repr_value(raw)
                    ),
                );
                refusal::with(&mut r, "verb", raw.clone());
                r
            }
        };
        Ok(Some(reply))
    }
}

// ── verbs ───────────────────────────────────────────────────────────

fn role_reply(meta: &Map<String, Value>, payload: &Value) -> Value {
    let Some(name) = payload
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return missing("role", "name");
    };
    let (cards, _) = cards_of(meta);
    match cards.iter().find(|c| c.name == name) {
        Some(c) => c.to_value(),
        None => refusal::refusal(
            refusal::NOT_FOUND,
            format!("role {} not found", py_repr(name)),
        ),
    }
}

/// A non-negative int, or a refusal.
fn count_arg(v: &Value, verb: &str, arg: &str) -> Result<u64, Value> {
    match v {
        Value::Number(n) if n.as_u64().is_some() => Ok(n.as_u64().unwrap_or(0)),
        _ => Err(bad(verb, arg, "a non-negative int")),
    }
}

async fn spawn_reply(
    reg: &AgentId,
    meta: &Map<String, Value>,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> Value {
    let Some(role) = payload
        .get("role")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return missing("spawn", "role");
    };
    let (cards, _) = cards_of(meta);
    let Some(card) = cards.iter().find(|c| c.name == role) else {
        return refusal::refusal(
            refusal::NOT_FOUND,
            format!("role {} not found", py_repr(role)),
        );
    };
    // the tier: a KEY the card names, resolved in config
    let tier_fields = match meta
        .get("tiers")
        .and_then(|t| t.get(&card.tier))
        .and_then(Value::as_object)
    {
        Some(t) => t.clone(),
        None => {
            let mut r = fail(
                "unknown_tier",
                format!(
                    "tier {} (named by role {}) is not in this registry's tiers",
                    py_repr(&card.tier),
                    py_repr(role)
                ),
            );
            r["tier"] = json!(card.tier);
            return r;
        }
    };
    if tier_fields
        .get("handler_module")
        .and_then(Value::as_str)
        .is_none()
    {
        return fail(
            "bad_tier",
            format!("tier {} has no handler_module", py_repr(&card.tier)),
        );
    }
    // the task: a string is the child's first message
    let first_message: Option<String> = match payload.get("task") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return bad("spawn", "task", "a string"),
    };
    let mut delegated = Map::new();
    if let Some(l) = payload.get("limits").filter(|l| !l.is_null()) {
        let Some(l) = l.as_object() else {
            return bad("spawn", "limits", "{total?, per_role?}");
        };
        for k in ["total", "per_role"] {
            if let Some(v) = l.get(k).filter(|v| !v.is_null()) {
                match count_arg(v, "spawn", &format!("limits.{k}")) {
                    Ok(n) => {
                        delegated.insert(k.to_string(), json!(n));
                    }
                    Err(e) => return e,
                }
            }
        }
    }
    let name = match payload.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(_) => return bad("spawn", "name", "a non-empty string"),
    };
    // `from`: branch the new agent off a fork point of an existing one
    let from: Option<(String, i64)> = match payload.get("from") {
        None | Some(Value::Null) => None,
        Some(f) => match (
            f.get("agent")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty()),
            f.get("at").and_then(Value::as_i64),
        ) {
            (Some(a), Some(at)) => Some((a.to_string(), at)),
            _ => return bad("spawn", "from", "{agent:str, at:int}"),
        },
    };

    let caller = current_origin();
    if let Some(refusal) = escalation(kernel, &caller, card) {
        return refusal;
    }
    let parent = caller.clone().unwrap_or_else(|| reg.clone());
    let all = spawned(kernel, reg);
    let limits = meta.get("limits").cloned().unwrap_or(Value::Null);

    // the registry's caps: per role, then in total
    let in_role = all.iter().filter(|s| s.role == role).count();
    if let Some(max) = cap(&limits, Some(role)) {
        if in_role as u64 >= max {
            return limit_refusal("role", role, in_role, max, None);
        }
    }
    let total = all.len();
    if let Some(max) = cap(&limits, None) {
        if total as u64 >= max {
            return limit_refusal("total", role, total, max, None);
        }
    }
    // delegated quotas: the caller and every lead above it
    let mut chain = vec![parent.clone()];
    chain.extend(ancestors(kernel, &parent));
    for lead in &chain {
        let Some(la) = agent_of(kernel, lead) else {
            continue;
        };
        let lm = meta_of(&la);
        if lm.get("spawned_by").and_then(Value::as_str) != Some(reg.as_str()) {
            continue;
        }
        let Some(q) = lm.get("limits") else { continue };
        let below: Vec<&Spawned> = all.iter().filter(|s| below_lead(kernel, s, lead)).collect();
        if let Some(max) = cap(q, None) {
            if below.len() as u64 >= max {
                return limit_refusal("subtree_total", role, below.len(), max, Some(lead.as_str()));
            }
        }
        if let Some(max) = q.get("per_role").and_then(Value::as_u64) {
            let n = below.iter().filter(|s| s.role == role).count();
            if n as u64 >= max {
                return limit_refusal("subtree_role", role, n, max, Some(lead.as_str()));
            }
        }
    }
    // what the caller may hand on: its remaining quota, less the sub-lead's own slot
    if !delegated.is_empty() && caller.is_some() {
        if let Some(q) = agent_of(kernel, &parent)
            .map(|a| meta_of(&a))
            .filter(|m| m.get("spawned_by").and_then(Value::as_str) == Some(reg.as_str()))
            .and_then(|m| m.get("limits").cloned())
        {
            let used = all
                .iter()
                .filter(|s| below_lead(kernel, s, &parent))
                .count() as u64;
            if let (Some(want), Some(max)) = (
                delegated.get("total").and_then(Value::as_u64),
                cap(&q, None),
            ) {
                let left = max.saturating_sub(used).saturating_sub(1);
                if want > left {
                    return limit_refusal(
                        "request",
                        role,
                        want as usize,
                        left,
                        Some(parent.as_str()),
                    );
                }
            }
            if let (Some(want), Some(max)) = (
                delegated.get("per_role").and_then(Value::as_u64),
                q.get("per_role").and_then(Value::as_u64),
            ) {
                if want > max {
                    return limit_refusal(
                        "request",
                        role,
                        want as usize,
                        max,
                        Some(parent.as_str()),
                    );
                }
            }
        }
    }

    if let Some((src, at)) = from {
        let ctx = SpawnFrom {
            reg,
            card,
            tier_fields: &tier_fields,
            caller: &caller,
            delegated: &delegated,
            name: &name,
            first_message,
        };
        return spawn_from(kernel, &ctx, &src, at).await;
    }

    // the child's grants: the card's, with `$registry` made real, plus `report`
    let mut can_call = card_can_call(reg, card);
    // a tier that names the store the agent keeps its conversation in: the agent may
    // use it (the other end is opened below, before the first message)
    let store = tier_fields
        .get("memory_store_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(s) = &store {
        grant_verbs(&mut can_call, s, &json!("*"));
    }
    let reg_verbs = reg_verbs_of(&can_call, reg);
    let mut callable_by = Map::new();
    if let Some(c) = &caller {
        callable_by.insert(c.as_str().to_string(), json!(PARENT_VERBS));
    }
    if first_message.is_some() {
        grant_verbs(&mut callable_by, reg.as_str(), &json!(["send"]));
    }

    let mut create = Map::new();
    for (k, v) in &tier_fields {
        create.insert(k.clone(), v.clone());
    }
    create.insert("type".to_string(), json!("create_agent"));
    create.insert("parent_id".to_string(), json!(parent.as_str()));
    create.insert("system_prompt".to_string(), json!(card.directive));
    create.insert("spawned_by".to_string(), json!(reg.as_str()));
    create.insert("role".to_string(), json!(role));
    create.insert("tier".to_string(), json!(card.tier));
    create.insert("can_call".to_string(), Value::Object(can_call));
    create.insert("callable_by".to_string(), Value::Object(callable_by));
    if let Some(n) = &name {
        create.insert("name".to_string(), json!(n));
    }
    if !delegated.is_empty() {
        create.insert("limits".to_string(), Value::Object(delegated));
    }
    let Some(root) = kernel.root() else {
        return fail("no_root", "the kernel has no root".to_string());
    };
    let made =
        fantastic_kernel::lifecycle::create_from_payload(kernel, &root, &Value::Object(create))
            .await;
    let Some(child) = made.get("id").and_then(Value::as_str).map(str::to_string) else {
        return made;
    };

    // the registry may hear the child's `report`; the parent controls its subtree
    let store_id = store
        .clone()
        .filter(|s| kernel.agents.contains_key(&AgentId::from(s.as_str())));
    if let Some(s) = store_id.clone() {
        let cid = child.clone();
        let _ = kernel.edit_callable_by(&AgentId::from(s.as_str()), move |m| {
            grant_verbs(m, &cid, &json!("*"));
            true
        });
    }
    let opened = open_edges(kernel, reg, caller.as_ref(), &child, &reg_verbs)
        .and_then(|()| open_tool_edges(kernel, &child, card));
    if let Err(e) = opened {
        undo_spawn(
            kernel,
            reg,
            caller.as_ref(),
            store_id.as_deref(),
            card,
            &child,
        )
        .await;
        return e;
    }
    if let Some(text) = first_message {
        send_first(kernel, reg, &child, text);
    }
    json!({"id": child, "role": role, "tier": card.tier})
}

/// The child's grants from its card: `$registry` made real, plus `report`.
fn card_can_call(reg: &AgentId, card: &Card) -> Map<String, Value> {
    let mut can_call = Map::new();
    for (k, v) in &card.grants {
        let key = if k == REGISTRY_TOKEN {
            reg.as_str()
        } else {
            k.as_str()
        };
        grant_verbs(&mut can_call, key, v);
    }
    grant_verbs(&mut can_call, reg.as_str(), &json!(["report"]));
    can_call
}

/// What the registry lets the child call on it.
fn reg_verbs_of(can_call: &Map<String, Value>, reg: &AgentId) -> Value {
    can_call
        .get(reg.as_str())
        .cloned()
        .unwrap_or(json!(["report"]))
}

/// The two edges every spawn opens: the registry accepts the child's calls, and the
/// caller (when there is one) controls the child.
fn open_edges(
    kernel: &Kernel,
    reg: &AgentId,
    caller: Option<&AgentId>,
    child: &str,
    reg_verbs: &Value,
) -> Result<(), Value> {
    let (child_id, verbs) = (child.to_string(), reg_verbs.clone());
    kernel
        .edit_callable_by(reg, move |m| {
            grant_verbs(m, &child_id, &verbs);
            true
        })
        .map_err(|e| fail("grant_failed", format!("spawn: {e}")))?;
    if let Some(c) = caller {
        let cid = child.to_string();
        kernel
            .edit_can_call(c, move |m| {
                m.insert(cid, json!(PARENT_VERBS));
                true
            })
            .map_err(|e| fail("grant_failed", format!("spawn: {e}")))?;
    }
    Ok(())
}

/// A string task: let the registry `send` and send it, DETACHED, as the registry
/// (a spawn answers at once, it never waits for the child's turn to end).
fn send_first(kernel: &Arc<Kernel>, reg: &AgentId, child: &str, text: String) {
    let cid = child.to_string();
    let _ = kernel.edit_can_call(reg, move |m| {
        grant_verbs(m, &cid, &json!(["send"]));
        true
    });
    let k = Arc::clone(kernel);
    let target = AgentId::from(child);
    fantastic_kernel::spawn_inherit(async move {
        let _ = k
            .send(vec![Item::new(
                target,
                json!({"type": "send", "text": text}),
            )])
            .await;
    });
}

/// Everything `spawn` has settled before it branches from a fork point.
struct SpawnFrom<'a> {
    reg: &'a AgentId,
    card: &'a Card,
    tier_fields: &'a Map<String, Value>,
    caller: &'a Option<AgentId>,
    delegated: &'a Map<String, Value>,
    name: &'a Option<String>,
    first_message: Option<String>,
}

/// The card's grants on tools: literal local agent ids only (not `$registry`, not
/// `$self`-like tokens, not `<guid>|x` peer keys).
fn tool_grants(card: &Card) -> Vec<(String, Value)> {
    card.grants
        .iter()
        .filter(|(k, _)| !k.starts_with('$') && !k.contains('|'))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// A card may only grant what its spawner holds: `None` when there is no caller (the
/// operator) or every verb the card lists on a tool is in the caller's own `can_call`;
/// otherwise the `not_allowed` refusal. `"*"` needs the caller to hold `"*"`.
fn escalation(kernel: &Kernel, caller: &Option<AgentId>, card: &Card) -> Option<Value> {
    let who = caller.as_ref()?;
    let holder = agent_of(kernel, who);
    let grant = holder
        .as_ref()
        .map(|w| meta_of(w).get("can_call").cloned().unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    let parent = holder
        .as_ref()
        .and_then(|w| w.parent_id.as_ref().map(|p| p.as_str().to_string()));
    for (tool, verbs) in tool_grants(card) {
        let listed: Vec<String> = match &verbs {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        let ancestors =
            kernel.ancestor_ids(agent_of(kernel, &AgentId::from(tool.as_str())).as_deref());
        for verb in &listed {
            let held = fantastic_kernel::authz::authorize_call(
                &grant,
                verb,
                &tool,
                who.as_str(),
                parent.as_deref(),
                &ancestors,
            )
            .allowed;
            if !held {
                return Some(fail(
                    "not_allowed",
                    format!(
                        "spawn: {who} may not grant {} on {} (role {})",
                        py_repr(verb),
                        py_repr(&tool),
                        py_repr(&card.name)
                    ),
                ));
            }
        }
    }
    None
}

/// Admit `child` on the tools its card grants (the tool's end of the edge; the
/// child's own end is its `can_call`). A tool that is not an agent is skipped. All or
/// nothing: on a refused edit the edits made are taken back.
fn open_tool_edges(kernel: &Kernel, child: &str, card: &Card) -> Result<(), Value> {
    let mut done: Vec<AgentId> = Vec::new();
    for (tool, verbs) in tool_grants(card) {
        let id = AgentId::from(tool.as_str());
        if agent_of(kernel, &id).is_none() {
            continue;
        }
        let cid = child.to_string();
        match kernel.edit_callable_by(&id, move |m| {
            grant_verbs(m, &cid, &verbs);
            true
        }) {
            Ok(_) => done.push(id),
            Err(e) => {
                tool_ungrant(kernel, child, &done);
                return Err(fail("grant_failed", format!("spawn: {e}")));
            }
        }
    }
    Ok(())
}

/// Take `child` back out of the `callable_by` of `tools`.
fn tool_ungrant(kernel: &Kernel, child: &str, tools: &[AgentId]) {
    for t in tools {
        let cid = child.to_string();
        let _ = kernel.edit_callable_by(t, move |m| m.remove(&cid).is_some());
    }
}

/// Take a half-made spawn back: its tool edges, its keys on the registry, the memory
/// store and the caller, then the agent itself.
async fn undo_spawn(
    kernel: &Arc<Kernel>,
    reg: &AgentId,
    caller: Option<&AgentId>,
    store: Option<&str>,
    card: &Card,
    child: &str,
) {
    let tools: Vec<AgentId> = tool_grants(card)
        .into_iter()
        .map(|(t, _)| AgentId::from(t.as_str()))
        .filter(|t| agent_of(kernel, t).is_some())
        .collect();
    tool_ungrant(kernel, child, &tools);
    if let Some(s) = store {
        let cid = child.to_string();
        let _ = kernel.edit_callable_by(&AgentId::from(s), move |m| m.remove(&cid).is_some());
    }
    let cid = child.to_string();
    let _ = kernel.edit_callable_by(reg, move |m| m.remove(&cid).is_some());
    if let Some(c) = caller {
        let cid = child.to_string();
        let _ = kernel.edit_can_call(c, move |m| m.remove(&cid).is_some());
    }
    if let Some(a) = agent_of(kernel, &AgentId::from(child)) {
        fantastic_kernel::lifecycle::cascade_delete(kernel, &a).await;
    }
}

/// Does `who` hold `fork` on `source` in its own `can_call`?
fn holds_fork(kernel: &Kernel, who: &AgentId, source: &Agent) -> bool {
    let Some(w) = agent_of(kernel, who) else {
        return false;
    };
    let grant = meta_of(&w).get("can_call").cloned().unwrap_or(Value::Null);
    fantastic_kernel::authz::authorize_call(
        &grant,
        "fork",
        source.id.as_str(),
        who.as_str(),
        w.parent_id.as_ref().map(AgentId::as_str),
        &kernel.ancestor_ids(Some(source)),
    )
    .allowed
}

/// Let the registry `fork` the source: returns what to put back afterwards
/// (the registry's `can_call` entry for the source, the source's `callable_by`
/// entry for the registry; `None` inside = there was none).
fn fork_grant(
    kernel: &Kernel,
    reg: &AgentId,
    src: &AgentId,
) -> Result<(Option<Value>, Option<Value>), String> {
    let (mut before_cc, mut before_cb) = (None, None);
    let s = src.as_str().to_string();
    kernel.edit_can_call(reg, |m| {
        before_cc = m.get(&s).cloned();
        grant_verbs(m, &s, &json!(["fork"]));
        true
    })?;
    let r = reg.as_str().to_string();
    if let Err(e) = kernel.edit_callable_by(src, |m| {
        before_cb = m.get(&r).cloned();
        grant_verbs(m, &r, &json!(["fork"]));
        true
    }) {
        fork_ungrant(kernel, reg, src, &(before_cc, None));
        return Err(e);
    }
    Ok((before_cc, before_cb))
}

/// Take back what [`fork_grant`] added.
fn fork_ungrant(
    kernel: &Kernel,
    reg: &AgentId,
    src: &AgentId,
    before: &(Option<Value>, Option<Value>),
) {
    let put = |m: &mut Map<String, Value>, key: &str, old: &Option<Value>| {
        match old {
            Some(v) => m.insert(key.to_string(), v.clone()),
            None => m.remove(key),
        };
        true
    };
    let s = src.as_str().to_string();
    let old = before.0.clone();
    let _ = kernel.edit_can_call(reg, move |m| put(m, &s, &old));
    let r = reg.as_str().to_string();
    let old = before.1.clone();
    let _ = kernel.edit_callable_by(src, move |m| put(m, &r, &old));
}

/// `spawn {role, from:{agent, at}}`: `fork {at}` of `src` makes the agent (it sits
/// beside its source, in the source's store); the role card and the
/// limits are then applied over the copy like any spawn.
async fn spawn_from(kernel: &Arc<Kernel>, c: &SpawnFrom<'_>, src: &str, at: i64) -> Value {
    let (reg, card) = (c.reg, c.card);
    let src_id = AgentId::from(src);
    let source = agent_of(kernel, &src_id);
    // the caller (the operator when there is none) must itself hold `fork` on the source
    if let (Some(who), Some(source)) = (c.caller.as_ref(), source.as_ref()) {
        if !holds_fork(kernel, who, source) {
            return fail(
                "not_allowed",
                format!("spawn: {who} may not fork {}", py_repr(src)),
            );
        }
    }
    let before = match fork_grant(kernel, reg, &src_id) {
        Ok(b) => b,
        Err(_) if source.is_none() => (None, None),
        Err(e) => return fail("grant_failed", format!("spawn: {e}")),
    };
    // the registry forks AS ITSELF; whatever the fork refuses passes through unchanged
    let out = kernel
        .send(vec![Item::new(
            src_id.clone(),
            json!({"type": "fork", "at": at}),
        )])
        .await
        .remove(0)
        .result;
    fork_ungrant(kernel, reg, &src_id, &before);
    let Some(child) = out.get("id").and_then(Value::as_str).map(str::to_string) else {
        return out;
    };
    let copied = out.get("copied").cloned().unwrap_or(Value::Null);
    let Some(fork) = agent_of(kernel, &AgentId::from(child.as_str())) else {
        return out;
    };

    // the copy is the source's whole record: overwrite what the role decides, drop the rest
    let fm = meta_of(&fork);
    let store = fm
        .get("memory_store_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut can_call = Map::new();
    if let Some(s) = &store {
        if let Some(v) = fm
            .get("can_call")
            .and_then(Value::as_object)
            .and_then(|m| m.get(s))
        {
            can_call.insert(s.clone(), v.clone());
        }
    }
    for (k, v) in card_can_call(reg, card) {
        grant_verbs(&mut can_call, &k, &v);
    }
    let reg_verbs = reg_verbs_of(&can_call, reg);
    let mut patch = Map::new();
    for (k, v) in c.tier_fields {
        // the fork stays in the source's store: its copied prefix must stay readable
        if k != "memory_store_id" && k != "handler_module" {
            patch.insert(k.clone(), v.clone());
        }
    }
    patch.insert("system_prompt".to_string(), json!(card.directive));
    patch.insert("spawned_by".to_string(), json!(reg.as_str()));
    patch.insert("role".to_string(), json!(card.name));
    patch.insert("tier".to_string(), json!(card.tier));
    patch.insert("can_call".to_string(), Value::Object(can_call));
    if let Some(n) = c.name {
        patch.insert("name".to_string(), json!(n));
        patch.insert("display_name".to_string(), json!(n));
    }
    let mut cleared: Vec<&str> = Vec::new();
    // a task line copied from the source record is dropped
    cleared.push("task");
    if c.delegated.is_empty() {
        cleared.push("limits");
    } else {
        patch.insert("limits".to_string(), Value::Object(c.delegated.clone()));
    }
    match c.caller {
        Some(who) => {
            patch.insert("lead".to_string(), json!(who.as_str()));
        }
        None => cleared.push("lead"),
    }
    {
        let mut m = fork.meta.write().expect("meta poisoned");
        for k in &cleared {
            m.remove(*k);
        }
    }
    kernel.update(&fork.id, patch);

    // the caller controls it; the registry hears its `report` and may `send` it
    let (caller, first) = (c.caller.clone(), c.first_message.is_some());
    let r = reg.as_str().to_string();
    let opened = kernel
        .edit_callable_by(&fork.id, move |m| {
            if let Some(w) = &caller {
                m.insert(w.as_str().to_string(), json!(PARENT_VERBS));
            }
            if first {
                grant_verbs(m, &r, &json!(["send"]));
            }
            true
        })
        .map_err(|e| fail("grant_failed", format!("spawn: {e}")))
        .and_then(|_| open_edges(kernel, reg, c.caller.as_ref(), &child, &reg_verbs))
        .and_then(|()| open_tool_edges(kernel, &child, card));
    if let Err(e) = opened {
        // nothing of the registry is left behind
        undo_spawn(
            kernel,
            reg,
            c.caller.as_ref(),
            store.as_deref(),
            card,
            &child,
        )
        .await;
        return e;
    }
    if let Some(text) = c.first_message.clone() {
        send_first(kernel, reg, &child, text);
    }
    json!({
        "id": child, "role": card.name, "tier": card.tier,
        "from": {"agent": src, "at": at}, "copied": copied,
    })
}

async fn report_reply(reg: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let state = match payload.get("state") {
        None | Some(Value::Null) => return missing("report", "state"),
        Some(Value::String(s)) if STATES.contains(&s.as_str()) => s.clone(),
        Some(_) => return bad("report", "state", &format!("one of {}", STATES.join("|"))),
    };
    let result = payload.get("result").cloned().unwrap_or(Value::Null);
    let me = current_origin();
    let Some(mine) = me.as_ref().and_then(|m| agent_of(kernel, m)) else {
        return fail(
            "not_spawned",
            "report: only an agent this registry spawned may report".to_string(),
        );
    };
    let mm = meta_of(&mine);
    if mm.get("spawned_by").and_then(Value::as_str) != Some(reg.as_str()) {
        return fail(
            "not_spawned",
            format!(
                "report: {} was not spawned by this registry",
                py_repr(mine.id.as_str())
            ),
        );
    }
    // the parent hears it; an emit is gated, so the registry is let in first
    if let Some(parent) = mine.parent_id.clone().filter(|p| p != reg) {
        let p = parent.as_str().to_string();
        let _ = kernel.edit_can_call(reg, move |m| {
            grant_verbs(m, &p, &json!(["emit"]));
            true
        });
        let r = reg.as_str().to_string();
        let _ = kernel.edit_callable_by(&parent, move |m| {
            grant_verbs(m, &r, &json!(["emit"]));
            true
        });
        kernel
            .emit(
                &parent,
                json!({"type": "report", "from": mine.id.as_str(), "state": state, "result": result}),
            )
            .await;
    }
    json!({"ok": true})
}

fn fleet_reply(reg: &AgentId, kernel: &Kernel) -> Value {
    let mut roles: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for s in spawned(kernel, reg) {
        roles
            .entry(s.role)
            .or_default()
            .push(json!({"id": s.id.as_str()}));
    }
    json!({"roles": roles})
}

fn set_limit_reply(
    reg: &AgentId,
    meta: &Map<String, Value>,
    payload: &Value,
    kernel: &Kernel,
) -> Value {
    let role = match payload.get("role") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(_) => return bad("set_limit", "role", "a non-empty string"),
    };
    let Some(max) = payload.get("max") else {
        return missing("set_limit", "max");
    };
    let max = match max {
        Value::Null => None,
        v => match count_arg(v, "set_limit", "max") {
            Ok(n) => Some(n),
            Err(e) => return e,
        },
    };
    let mut limits = meta
        .get("limits")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    match (&role, max) {
        (None, Some(n)) => {
            limits.insert("total".to_string(), json!(n));
        }
        (None, None) => {
            limits.remove("total");
        }
        (Some(r), m) => {
            let mut per = limits
                .get("per_role")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            match m {
                Some(n) => per.insert(r.clone(), json!(n)),
                None => per.remove(r),
            };
            if per.is_empty() {
                limits.remove("per_role");
            } else {
                limits.insert("per_role".to_string(), Value::Object(per));
            }
        }
    }
    let mut patch = Map::new();
    patch.insert("limits".to_string(), Value::Object(limits));
    kernel.update(reg, patch);
    json!({"ok": true, "role": role, "max": max})
}

fn get_count_reply(
    reg: &AgentId,
    meta: &Map<String, Value>,
    payload: &Value,
    kernel: &Kernel,
) -> Value {
    let role = match payload.get("role") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
        Some(_) => return bad("get_count", "role", "a non-empty string"),
    };
    let running = spawned(kernel, reg)
        .iter()
        .filter(|s| role.is_none_or(|r| s.role == r))
        .count();
    let max = meta.get("limits").and_then(|l| cap(l, role));
    json!({"running": running, "max": max})
}

#[cfg(test)]
mod tests;
