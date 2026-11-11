//! What an assistant row keeps about its own request (#618, #619), in its `attributes`
//! map: the request's `parts` as a DELTA against the chat's previous request (a full
//! list on a checkpoint), the running `totals`, and the `cost` of the pass.
//!
//! None of it reaches the model: the request is built from role and content only
//! ([`crate::agent_loop::render_for_model`]).

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Every this many requests the parts are a full list again (a checkpoint), so a
/// reader never replays more than this many deltas.
pub(crate) const CHECKPOINT_EVERY: u64 = 50;

/// The size cap of a row's `attributes` at write, parts excluded (they are bounded by
/// the delta / checkpoint design): an oversized map is refused.
pub(crate) const ATTRIBUTES_CAP: usize = 8 * 1024;

/// What the chat remembers of its previous request, in RAM: lost on a restart and
/// cleared by a compaction, and the next request is then a checkpoint.
#[derive(Clone, Debug)]
pub(crate) struct PrevRequest {
    /// The assistant row that request earned.
    pub(crate) row_id: i64,
    /// Its full parts list.
    pub(crate) list: Vec<Value>,
    /// Requests since the last checkpoint (the checkpoint itself is 0).
    pub(crate) n: u64,
    /// The running totals after it.
    pub(crate) totals: Value,
}

/// A short stable hash of `text`: the first 16 hex digits of its SHA-256.
pub(crate) fn short_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The digest of a full parts list.
pub(crate) fn digest(list: &[Value]) -> String {
    short_hash(&Value::Array(list.to_vec()).to_string())
}

/// The identity of each entry of a list: `{part, ref?, n?}`. A row is its `ref`; a
/// system piece is its name, `n` counting a name that comes again (a prompt wrapped by
/// two `system_text` pieces).
fn idents(list: &[Value]) -> Vec<Value> {
    let mut seen: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    list.iter()
        .map(|e| {
            let mut id = Map::new();
            id.insert("part".into(), e["part"].clone());
            if let Some(r) = e.get("ref") {
                id.insert("ref".into(), r.clone());
            } else {
                let c = seen.entry(e["part"].to_string()).or_insert(0);
                if *c > 0 {
                    id.insert("n".into(), json!(*c));
                }
                *c += 1;
            }
            Value::Object(id)
        })
        .collect()
}

/// The delta that turns `prev` into `full`, or `None` when it cannot say it (the
/// entries both lists keep come in another order): the caller writes a checkpoint.
fn delta(prev_id: i64, prev: &[Value], full: &[Value]) -> Option<Value> {
    let (pi, fi) = (idents(prev), idents(full));
    let key = |v: &Value| v.to_string();
    let prev_at: std::collections::HashMap<String, usize> =
        pi.iter().enumerate().map(|(i, v)| (key(v), i)).collect();
    let full_keys: std::collections::HashSet<String> = fi.iter().map(key).collect();
    let kept_prev: Vec<String> = pi
        .iter()
        .map(key)
        .filter(|k| full_keys.contains(k))
        .collect();
    let kept_full: Vec<String> = fi
        .iter()
        .map(key)
        .filter(|k| prev_at.contains_key(k))
        .collect();
    if kept_prev != kept_full {
        return None;
    }
    let (mut add, mut change) = (Vec::new(), Vec::new());
    for (i, e) in full.iter().enumerate() {
        match prev_at.get(&key(&fi[i])) {
            None => {
                let mut a = e.clone();
                a["at"] = json!(i);
                add.push(a);
            }
            Some(&j) => {
                let old = &prev[j];
                let mut c = fi[i].as_object().cloned().unwrap_or_default();
                let mut changed = false;
                for field in ["form", "hash", "bytes"] {
                    if e.get(field) != old.get(field) {
                        if let Some(v) = e.get(field) {
                            c.insert(field.into(), v.clone());
                            changed = true;
                        }
                    }
                }
                if changed {
                    change.push(Value::Object(c));
                }
            }
        }
    }
    let drop: Vec<Value> = pi
        .iter()
        .filter(|v| !full_keys.contains(&key(v)))
        .cloned()
        .collect();
    Some(json!({
        "prev": prev_id,
        "add": add,
        "change": change,
        "drop": drop,
        "digest": digest(full),
    }))
}

/// The full list a `parts` value stands for, given the list it is a delta against
/// (ignored by a checkpoint).
#[cfg(test)]
pub(crate) fn apply(prev: &[Value], parts: &Value) -> Vec<Value> {
    if parts["full"] == json!(true) {
        return parts["list"].as_array().cloned().unwrap_or_default();
    }
    let drops: Vec<String> = parts["drop"]
        .as_array()
        .map(|a| a.iter().map(Value::to_string).collect())
        .unwrap_or_default();
    let changes: Vec<&Value> = parts["change"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let mut out: Vec<Value> = Vec::new();
    for (e, id) in prev.iter().zip(idents(prev)) {
        if drops.contains(&id.to_string()) {
            continue;
        }
        let mut e = e.clone();
        if let Some(c) = changes.iter().find(|c| {
            let mut cid: Value = (**c).clone();
            if let Some(m) = cid.as_object_mut() {
                m.retain(|k, _| matches!(k.as_str(), "part" | "ref" | "n"));
            }
            cid == id
        }) {
            for field in ["form", "hash", "bytes"] {
                if let Some(v) = c.get(field) {
                    e[field] = v.clone();
                }
            }
        }
        out.push(e);
    }
    for a in parts["add"].as_array().into_iter().flatten() {
        let at = a["at"].as_u64().unwrap_or(0) as usize;
        let mut e = a.clone();
        if let Some(m) = e.as_object_mut() {
            m.remove("at");
        }
        out.insert(at.min(out.len()), e);
    }
    out
}

/// The running totals after this request: `base` (the previous row's, or empty) plus
/// this request's bytes per part, and the fold counts read off what changed since the
/// previous list: `folds` (a row that was whole is now reduced), `recalls` (a reduced
/// row is whole again), `refolds` (a reduced row is reduced another way).
fn next_totals(base: &Value, prev: Option<&[Value]>, full: &[Value]) -> Value {
    let count = |k: &str| base.get(k).and_then(Value::as_u64).unwrap_or(0);
    let mut bytes: Map<String, Value> = base
        .get("bytes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for e in full {
        let name = e["part"].as_str().unwrap_or("").to_string();
        let sum = bytes.get(&name).and_then(Value::as_u64).unwrap_or(0)
            + e["bytes"].as_u64().unwrap_or(0);
        bytes.insert(name, json!(sum));
    }
    let (mut folds, mut recalls, mut refolds) =
        (count("folds"), count("recalls"), count("refolds"));
    if let Some(prev) = prev {
        let form_of: std::collections::HashMap<String, &str> = idents(prev)
            .iter()
            .zip(prev)
            .filter_map(|(id, e)| Some((id.to_string(), e.get("form")?.as_str()?)))
            .collect();
        for (id, e) in idents(full).iter().zip(full) {
            let (Some(old), Some(new)) = (
                form_of.get(&id.to_string()),
                e.get("form").and_then(Value::as_str),
            ) else {
                continue;
            };
            match (*old == "whole", new == "whole") {
                (true, false) => folds += 1,
                (false, true) => recalls += 1,
                (false, false) if *old != new => refolds += 1,
                _ => {}
            }
        }
    }
    json!({
        "requests": count("requests") + 1,
        "bytes": bytes,
        "folds": folds,
        "recalls": recalls,
        "refolds": refolds,
    })
}

/// The `parts` and `totals` of the request `full`, which the row `row_id` earns, and
/// what the chat remembers of it. A checkpoint (a full list) on the chat's first
/// request, the first after a restart or a compaction (`prev` is `None`), and every
/// [`CHECKPOINT_EVERY`] requests. `seed` is the totals of the last stored assistant row
/// when `prev` is `None` (a restart keeps counting).
pub(crate) fn build(
    prev: Option<&PrevRequest>,
    seed: Option<Value>,
    row_id: i64,
    full: Vec<Value>,
) -> (Value, Value, PrevRequest) {
    let base = prev
        .map(|p| p.totals.clone())
        .or(seed)
        .unwrap_or_else(|| json!({}));
    let totals = next_totals(&base, prev.map(|p| p.list.as_slice()), &full);
    let step = prev
        .filter(|p| p.n + 1 < CHECKPOINT_EVERY)
        .and_then(|p| delta(p.row_id, &p.list, &full).map(|d| (d, p.n + 1)));
    let (parts, n) = step.unwrap_or_else(|| {
        (
            json!({"full": true, "list": full, "digest": digest(&full)}),
            0,
        )
    });
    let next = PrevRequest {
        row_id,
        list: full,
        n,
        totals: totals.clone(),
    };
    (parts, totals, next)
}

/// The totals of the newest assistant row the chat holds, if it has one.
pub(crate) async fn seed_totals(self_id: &AgentId, kernel: &Arc<Kernel>) -> Option<Value> {
    crate::memory::load_history(self_id, kernel, false)
        .await
        .ok()?
        .iter()
        .rev()
        .find(|r| r.get("role").and_then(Value::as_str) == Some("assistant"))?
        .get("attributes")?
        .get("totals")
        .cloned()
}

/// `model`'s entry in a provider record's `prices`: its own, else the entry of the id
/// it is a dated form of (the longest).
fn price_entry<'a>(meta: &'a Map<String, Value>, model: &str) -> Option<&'a Value> {
    let prices = meta.get("prices")?.as_object()?;
    prices
        .get(model)
        .or_else(|| {
            prices
                .iter()
                .filter(|(id, _)| {
                    model
                        .strip_prefix(id.as_str())
                        .is_some_and(|rest| rest.starts_with('-'))
                })
                .max_by_key(|(id, _)| id.len())
                .map(|(_, v)| v)
        })
        .filter(|v| v.is_object())
}

/// The cost of one pass: `{usd, prices}`, the price entry of the serving provider's
/// record kept with it (USD per million tokens: `input`, `output`, and `cache_read` /
/// `cache_write_5m` for the cache counts the pass has), or `{unpriced: true}` when a
/// price the pass needs is missing: never 0. `None` when the pass reported no usage.
pub(crate) fn cost(
    usage: Option<&Value>,
    provider_meta: &Map<String, Value>,
    model: &str,
) -> Option<Value> {
    let usage = usage?;
    let unpriced = json!({"unpriced": true});
    let Some(entry) = price_entry(provider_meta, model) else {
        return Some(unpriced);
    };
    let n = |u: &str| usage.get(u).and_then(Value::as_f64);
    let p = |k: &str| entry.get(k).and_then(Value::as_f64);
    let paid = |count: Option<f64>, price: Option<f64>| match count {
        None => Some(0.0),
        Some(c) => price.map(|p| c * p),
    };
    let sum = (|| {
        let input = n("input")? * p("input")?;
        let output = n("output")? * p("output")?;
        let read = paid(n("cache_read"), p("cache_read"))?;
        let write = paid(n("cache_write"), p("cache_write_5m"))?;
        Some((input + output + read + write) / 1e6)
    })();
    Some(match sum {
        Some(usd) => json!({"usd": usd, "prices": entry}),
        None => unpriced,
    })
}

/// `row` with `attributes.source` set to `source` (the agent that sent it; a peer
/// kernel's agent keeps its `<guid>|<id>` form).
pub(crate) fn with_source(mut row: Value, source: &str) -> Value {
    if source.is_empty() {
        return row;
    }
    if let Some(m) = row.as_object_mut() {
        let a = m.entry("attributes").or_insert_with(|| json!({}));
        if let Some(a) = a.as_object_mut() {
            a.insert("source".into(), json!(source));
        }
    }
    row
}

/// Refuses a row whose `attributes` (parts excluded) pass [`ATTRIBUTES_CAP`]; the
/// error names the agent's store, the row and the field.
pub(crate) fn check_cap(self_id: &AgentId, row: &Map<String, Value>) -> Result<(), String> {
    let Some(a) = row.get("attributes").and_then(Value::as_object) else {
        return Ok(());
    };
    let size: usize = a
        .iter()
        .filter(|(k, _)| k.as_str() != "parts")
        .map(|(k, v)| k.len() + v.to_string().len())
        .sum();
    if size > ATTRIBUTES_CAP {
        return Err(format!(
            "save_history: the attributes of row {} in the store of {} are {size} bytes, over the cap of {ATTRIBUTES_CAP} (parts excluded)",
            row.get("id").map_or_else(|| "?".to_string(), Value::to_string),
            self_id.as_str()
        ));
    }
    Ok(())
}
