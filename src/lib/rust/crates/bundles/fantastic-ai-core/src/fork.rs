//! `fork`, `mark`, `fork_points` — start a new agent from the first N rows of one
//! (#425). Python `ai_core/core.py` `_fork`, `_mark`, `_fork_points`, rule for rule.
//!
//! THE PREFIX IS A COPY. Rows are never edited and the provider's prompt cache
//! compares bytes, not storage, so the fork re-appends the parent's rows under its
//! OWN key with their original ids and hits the parent's cache. The new agent is the
//! parent's card (handler, model, endpoint, system prompt, `memory_store_id`,
//! context settings, `can_call`) with the store's `callable_by` opened for it. The
//! store only serves a caller its own key, so the copy is written AS the fork.
//!
//! A FORK POINT IS ALWAYS A TURN BOUNDARY OUTSIDE A FOLD. `fork {at}` refuses an
//! `at` that names no row (`not_found`), cuts a turn or a fold (`bad_arg`), and a parent
//! that is mid-turn (`locked`). `mark` writes a
//! `mark` row only where `fork` would accept it, so a marked point always forks.

use std::collections::HashSet;
use std::sync::Arc;

use fantastic_kernel::refusal::{bad, BAD_ARG, LOCKED, NOT_FOUND, UNKNOWN_AGENT};
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};

use crate::constants::{CACHE_LIFETIME_SECS, MAX_MARKS};
use crate::folds::{chains, is_fold, is_open, refs_of, row_id};
use crate::helpers::{agent_meta, now_secs};
use crate::memory::{
    rows_of, save_history, store_call, store_of, MemResult, BOOKKEEPING, TURN_ROLES,
};

/// Rows that belong to the live state of ONE agent, never to a prefix a fork
/// inherits: the pause state, the messages held for it, a late arrival and the marks
/// (each fork writes its own).
const NOT_COPIED: [&str; 4] = ["pause", "held", "late", "mark"];

fn kind_of(r: &Value) -> Option<&str> {
    r.get("kind").and_then(Value::as_str)
}

/// A refusal with its named `reason`, the `at` it judged and what to do instead.
fn refuse(verb: &str, code: &str, why: &str, hint: &str, at: Option<i64>) -> Value {
    let mut v = fantastic_kernel::refusal::refusal_hint(code, format!("{verb}: {why}"), hint);
    if let Some(at) = at {
        fantastic_kernel::refusal::with(&mut v, "at", Value::from(at));
    }
    v
}

/// A turn is running for this agent, or its chain is alive: its rows are moving.
fn mid_turn(agent_id: &AgentId) -> bool {
    let st = crate::state::state_for(agent_id);
    st.chain.lock().expect("chain poisoned").alive || crate::state::is_generating(agent_id)
}

/// A conversation row: a chat turn (a fold included), not bookkeeping and not
/// one of the agent's own live-state rows.
fn is_conversation(r: &Value) -> bool {
    row_id(r).is_some()
        && !kind_of(r).is_some_and(|k| BOOKKEEPING.contains(&k) || NOT_COPIED.contains(&k))
        && r.get("role")
            .and_then(Value::as_str)
            .is_some_and(|role| TURN_ROLES.contains(&role))
}

/// Every leaf (non-fold) id under a fold, through nested folds.
fn leaves(fold: &Value, folds: &[&Value], seen: &mut HashSet<i64>, out: &mut Vec<i64>) {
    for m in refs_of(fold) {
        match folds.iter().find(|f| row_id(f) == Some(m)) {
            Some(inner) => {
                if seen.insert(m) {
                    leaves(inner, folds, seen, out);
                }
            }
            None => out.push(m),
        }
    }
}

/// Why `at` is not a fork point: the kernel refusal code it answers with, what is
/// wrong, and what to do instead.
struct NoFork {
    code: &'static str,
    why: String,
    hint: &'static str,
}

/// The rows a fork at `at` carries over, in id order — or why `at` is no fork
/// point. `rows` is the parent's WHOLE store.
fn plan(rows: &[Value], at: i64) -> Result<Vec<Value>, NoFork> {
    let conv: Vec<&Value> = rows.iter().filter(|r| is_conversation(r)).collect();
    if !conv.iter().any(|r| row_id(r) == Some(at)) {
        return Err(NoFork {
            code: NOT_FOUND,
            why: format!("no row {at} in this conversation"),
            hint: "name a row of this conversation (see `history`)",
        });
    }
    // Folds first: a fold whose members straddle `at` is cut, whatever else holds.
    let folds: Vec<&Value> = conv.iter().copied().filter(|r| is_fold(r)).collect();
    let mut whole: HashSet<i64> = HashSet::new();
    for f in &folds {
        let id = row_id(f).unwrap_or_default();
        let mut ls = Vec::new();
        leaves(f, &folds, &mut HashSet::from([id]), &mut ls);
        let below = ls.iter().any(|&l| l <= at);
        let above = ls.iter().any(|&l| l > at);
        if above && (below || id <= at) {
            return Err(NoFork {
                code: BAD_ARG,
                why: format!("row {at} cuts fold {id}: some of its members are before it, some after"),
                hint: "fork at a row before the fold or after all of it",
            });
        }
        if id > at && below {
            whole.insert(id);
        }
    }
    // A turn is a chain of rows linked by `parent`: `at` may not fall inside one,
    // and an open chain (a reply still owed) is not yet a turn. A fold the fork takes
    // whole sits where its members sat, so it is not a row above `at`.
    let in_chain: Vec<Value> = conv
        .iter()
        .filter(|r| !row_id(r).is_some_and(|id| whole.contains(&id)))
        .map(|r| (*r).clone())
        .collect();
    for chain in chains(&in_chain) {
        let below = chain
            .iter()
            .any(|&i| row_id(&in_chain[i]).is_some_and(|id| id <= at));
        let above = chain
            .iter()
            .any(|&i| row_id(&in_chain[i]).is_some_and(|id| id > at));
        if below && (above || is_open(&in_chain, &chain)) {
            return Err(NoFork {
                code: BAD_ARG,
                why: format!("row {at} is inside a turn that is not finished"),
                hint: "fork at the end of a finished turn, or at a `mark`",
            });
        }
    }
    // What the prompt is assembled from: the newest `view` row at or
    // before `at`, so the fork's prompt is the parent's for the same prefix.
    let newest = |k: &str| {
        rows.iter()
            .filter(|r| kind_of(r) == Some(k))
            .filter_map(row_id)
            .filter(|&id| id <= at)
            .max()
    };
    let view = newest("view");
    let mut out: Vec<Value> = rows
        .iter()
        .filter(|r| {
            let Some(id) = row_id(r) else { return false };
            let kind = kind_of(r);
            if whole.contains(&id) {
                return true;
            }
            if kind == Some("view") {
                return Some(id) == view;
            }
            id <= at && !kind.is_some_and(|k| NOT_COPIED.contains(&k))
        })
        .map(|r| {
            let mut m: Map<String, Value> = r.as_object().cloned().unwrap_or_default();
            m.remove("agent");
            Value::Object(m)
        })
        .collect();
    out.sort_by_key(|r| row_id(r).unwrap_or_default());
    Ok(out)
}

/// The parent's whole store, in id order.
async fn whole_store(agent_id: &AgentId, kernel: &Arc<Kernel>) -> MemResult<Vec<Value>> {
    let res = store_call(agent_id, kernel, json!({"type": "read"})).await?;
    Ok(rows_of(&res, agent_id))
}

/// `fork {at, name?, id?}`: a new agent from this one's rows up to `at`. Returns
/// `{id, copied, at}`. `from:{agent, at}` runs the same verb on THAT agent, as a
/// gated send of this one.
///
/// # Errors
/// The memory store refused a read of the parent's rows.
pub async fn fork(
    agent_id: &AgentId,
    args: &crate::declared::Fork,
    kernel: &Arc<Kernel>,
) -> MemResult<Value> {
    let verb = "fork";
    if let Some(from) = &args.from {
        return Ok(fork_from(args, from, kernel).await);
    }
    let Some(at) = args.at else {
        return Ok(fantastic_kernel::refusal::missing(verb, "at"));
    };
    let (name, want_id) = (args.name.clone(), args.id.clone());
    // A turn holds the FIFO lock for its whole run: the rows must not move under the copy.
    let st = crate::state::state_for(agent_id);
    let lock = Arc::clone(&st.lock);
    let Ok(_turn) = lock.try_lock() else {
        return Ok(parent_mid_turn(agent_id, at));
    };
    if mid_turn(agent_id) {
        return Ok(parent_mid_turn(agent_id, at));
    }
    let rows = whole_store(agent_id, kernel).await?;
    let copy = match plan(&rows, at) {
        Ok(c) => c,
        Err(no) => return Ok(refuse(verb, no.code, &no.why, no.hint, Some(at))),
    };
    let store = store_of(agent_id, kernel)?;
    let Some(parent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
        return Ok(refuse(
            verb,
            UNKNOWN_AGENT,
            "the agent is gone",
            "",
            Some(at),
        ));
    };
    // THE CARD: the parent's record, minus what is the parent's alone.
    let mut meta = agent_meta(agent_id, kernel);
    for k in ["callable_by", "delete_lock"] {
        meta.remove(k);
    }
    let shown = parent.name_or_id();
    meta.insert(
        "display_name".to_string(),
        Value::from(name.unwrap_or_else(|| format!("{shown} fork"))),
    );
    let Some(handler_module) = parent.handler_module.clone() else {
        return Ok(refuse(
            verb,
            UNKNOWN_AGENT,
            "the agent runs no bundle",
            "",
            Some(at),
        ));
    };
    let create = fantastic_kernel::declared::kernel::CreateAgent {
        handler_module,
        parent_id: parent
            .parent_id
            .as_ref()
            .map_or("kernel", |p| p.as_str())
            .to_string(),
        id: want_id,
        fields: meta,
    };
    let Some(root) = kernel.root() else {
        return Ok(refuse(
            verb,
            "no_root",
            "the kernel has no root",
            "",
            Some(at),
        ));
    };
    let made = fantastic_kernel::lifecycle::create_agent(kernel, &root, create).await;
    if made.get("error").is_some_and(|e| !e.is_null()) {
        let mut m = made;
        m["at"] = Value::from(at);
        return Ok(m);
    }
    let new_id = AgentId::from(made.get("id").and_then(Value::as_str).unwrap_or_default());
    // THE GRANT: the store admits the fork the way it admits the parent.
    let _ = kernel.edit_callable_by(&store, |cb| {
        let mode = cb
            .get(agent_id.as_str())
            .cloned()
            .unwrap_or_else(|| json!("*"));
        cb.insert(new_id.0.clone(), mode);
        true
    });
    // THE COPY: ONE `append`, as the fork (the store keys a row by its writer), ids
    // kept, all or nothing.
    let n = copy.len();
    let wrote = fantastic_kernel::send::as_agent(
        &new_id,
        store_call(&new_id, kernel, json!({"type": "append", "rows": copy})),
    )
    .await;
    if let Err(e) = wrote {
        // The half-made agent is deleted, its grant taken back; the store keeps none of it.
        if let Some(agent) = kernel.agents.get(&new_id).map(|a| Arc::clone(&a)) {
            fantastic_kernel::lifecycle::cascade_delete(kernel, &agent).await;
        }
        let _ = kernel.edit_callable_by(&store, |cb| cb.remove(new_id.as_str()).is_some());
        let v = refuse(
            verb,
            "store_refused",
            &format!("the memory store refused the copy: {e}"),
            "the store must let the fork `append` and `read` as it lets this agent",
            Some(at),
        );
        return Ok(v);
    }
    Ok(json!({"id": new_id.0, "copied": n, "at": at}))
}

fn parent_mid_turn(agent_id: &AgentId, at: i64) -> Value {
    refuse(
        "fork",
        LOCKED,
        &format!("a turn is running for {agent_id}"),
        "fork once the running turn has answered, or at a `mark`",
        Some(at),
    )
}

/// `fork {from:{agent, at}}`: ask THAT agent to fork itself at `at`.
async fn fork_from(
    args: &crate::declared::Fork,
    from: &crate::declared::ForkFrom,
    kernel: &Arc<Kernel>,
) -> Value {
    let mut p = Map::new();
    p.insert("type".into(), json!("fork"));
    p.insert("at".into(), json!(from.at));
    for (k, v) in [("name", &args.name), ("id", &args.id)] {
        if let Some(v) = v {
            p.insert(k.into(), json!(v));
        }
    }
    let out = kernel
        .send(vec![Item::new(AgentId::from(from.agent.as_str()), Value::Object(p))])
        .await
        .remove(0);
    out.result
}

/// `mark {label, note?, version?}`: mark this agent's own history at a turn
/// boundary, outside any fold — a point `fork` always accepts. Keeps the last
/// [`MAX_MARKS`]. Returns `{marked, at, label}`.
///
/// # Errors
/// The memory store refused a read or a write.
pub async fn mark(
    agent_id: &AgentId,
    args: &crate::declared::Mark,
    kernel: &Arc<Kernel>,
) -> MemResult<Value> {
    let verb = "mark";
    if args.label.trim().is_empty() {
        return Ok(bad(verb, "label", "a non-empty string"));
    }
    let label = args.label.clone();
    let (note, version) = (args.note.clone(), args.version.clone());
    let st = crate::state::state_for(agent_id);
    let lock = Arc::clone(&st.lock);
    let busy = || {
        refuse(
            verb,
            LOCKED,
            &format!("a turn is running for {agent_id}"),
            "mark once the running turn has answered",
            None,
        )
    };
    let Ok(_turn) = lock.try_lock() else {
        return Ok(busy());
    };
    if mid_turn(agent_id) {
        return Ok(busy());
    }
    let rows = whole_store(agent_id, kernel).await?;
    let Some(at) = rows
        .iter()
        .filter(|r| is_conversation(r))
        .filter_map(row_id)
        .max()
    else {
        return Ok(refuse(
            verb,
            NOT_FOUND,
            "there is nothing in the conversation to mark",
            "mark after the first turn",
            None,
        ));
    };
    if let Err(no) = plan(&rows, at) {
        return Ok(refuse(
            verb,
            BAD_ARG,
            &no.why,
            "mark once the turn has ended",
            Some(at),
        ));
    }
    let ids = save_history(
        agent_id,
        kernel,
        &[json!({"kind": "mark", "label": label, "note": note, "version": version, "at": at})],
    )
    .await?;
    let marked = ids.first().copied().unwrap_or_default();
    // THE LAST N: older marks go in the same step.
    let mut marks: Vec<i64> = rows
        .iter()
        .filter(|r| kind_of(r) == Some("mark"))
        .filter_map(row_id)
        .collect();
    marks.push(marked);
    if marks.len() > MAX_MARKS {
        let doomed: Vec<i64> = marks[..marks.len() - MAX_MARKS].to_vec();
        store_call(agent_id, kernel, json!({"type": "prune", "ids": doomed})).await?;
        // The head cache holds what the store just dropped: read it afresh.
        crate::memory::forget_head(agent_id, kernel);
    }
    Ok(json!({"marked": marked, "at": at, "label": label}))
}

/// `fork_points {filter?}`: THIS agent's marks, `{agent, at, label, note, version,
/// rows_after, model, warm}` each. `filter` is `{label?, version?}` (a substring of
/// the label, the exact version). Returns `{points}`.
///
/// # Errors
/// The memory store refused the read.
pub async fn fork_points(
    agent_id: &AgentId,
    args: &crate::declared::ForkPoints,
    kernel: &Arc<Kernel>,
) -> MemResult<Value> {
    let want_label = args.filter.as_ref().and_then(|f| f.label.as_deref());
    let want_version = args.filter.as_ref().and_then(|f| f.version.as_deref());
    let rows = whole_store(agent_id, kernel).await?;
    let model = agent_meta(agent_id, kernel)
        .get("model")
        .cloned()
        .unwrap_or(Value::Null);
    let warm = crate::state::state_for(agent_id)
        .last_send_secs
        .lock()
        .expect("last_send poisoned")
        .is_some_and(|t| now_secs() - t < CACHE_LIFETIME_SECS);
    let points: Vec<Value> = rows
        .iter()
        .filter(|r| kind_of(r) == Some("mark"))
        .filter(|m| {
            let label = m.get("label").and_then(Value::as_str).unwrap_or("");
            let version = m.get("version").and_then(Value::as_str);
            want_label.is_none_or(|w| label.contains(w))
                && want_version.is_none_or(|w| version == Some(w))
        })
        .map(|m| {
            let at = m.get("at").and_then(Value::as_i64).unwrap_or_default();
            let after = rows
                .iter()
                .filter(|r| is_conversation(r))
                .filter(|r| row_id(r).is_some_and(|id| id > at))
                .count();
            json!({
                "agent": agent_id.as_str(),
                "at": at,
                "label": m.get("label").cloned().unwrap_or(Value::Null),
                "note": m.get("note").cloned().unwrap_or(Value::Null),
                "version": m.get("version").cloned().unwrap_or(Value::Null),
                "rows_after": after,
                "model": model,
                "warm": warm,
            })
        })
        .collect();
    Ok(json!({"points": points}))
}
