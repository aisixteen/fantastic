//! HOW TO CALL an agent — the verb docs a `reflect` carries, and the reflect
//! subtree filters (python `kernel/_docs.py`).
//!
//! A reflect says what an agent IS and which verbs it answers, one line each. How
//! to call it in full is the same reflect with `tier`, `verb` or `part`
//! (`section`): rendered from the agent's declared verbs, in two tiers —
//! `oneliners` (default), each verb's signature line; `detailed`, the whole text —
//! with each verb's JSON schema by `part`. Without a `verb` filter the section also
//! carries the bundle's one short `man` text — the only prose a bundle has;
//! everything else is rendered from the declarations. Only the verbs the reflect
//! shows its caller are documented.

use crate::agent::Agent;
use crate::declared::common::Reflect;
use crate::declared::{Part, Tier};
use crate::kernel::Kernel;
use crate::refusal::{refusal_hint, with, BAD_ARG};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;

/// One row of a verb docs answer: the verb, its ONE-LINE text and its DETAILED text.
pub type Entry = (String, String, String);

/// The rows of DECLARED verbs, rendered from the declarations ([`crate::verbs`]): the
/// lists in order, then the agent's dynamic verbs; a verb named earlier is not
/// repeated.
#[must_use]
pub fn entries_of_decls(
    lists: &[&[crate::verbs::Verb]],
    dynamic: &[crate::verbs::DynVerb],
) -> Vec<Entry> {
    crate::verbs::rows(lists, dynamic)
        .iter()
        .map(|r| {
            (
                r.name.to_string(),
                crate::verbs::line(r),
                crate::verbs::detail(r),
            )
        })
        .collect()
}

/// The VERB SECTION of a reflect of `id`, over the verbs it shows (`rows`), as the
/// keys to add to the reply. With none of `tier`, `verb` and `part`: `verbs`, one
/// line each. With any of them, the verb docs: `part` `docs` gives the text (`tier`,
/// `verbs`, and `man` when no `verb` is asked for), `schema` gives `schemas`, `all`
/// (the default) both. `Err` is the refusal of a `verb` that is not among `rows`,
/// naming only those.
pub(crate) fn section(
    id: &str,
    args: &Reflect,
    rows: &[crate::verbs::Row],
    man: Option<&str>,
) -> Result<Map<String, Value>, Value> {
    let mut out = Map::new();
    if args.tier.is_none() && args.verb.is_none() && args.part.is_none() {
        out.insert("verbs".into(), crate::verbs::lines(rows));
        return Ok(out);
    }
    let tier = args.tier.unwrap_or(Tier::Oneliners);
    let part = args.part.unwrap_or(Part::All);
    let verb = args.verb.as_deref();
    if let Some(v) = verb {
        if !rows.iter().any(|r| r.name == v) {
            let names: Vec<&str> = rows.iter().map(|r| r.name).collect();
            let mut r = refusal_hint(
                BAD_ARG,
                format!("reflect: {id} has no verb {}", fantastic_bundle::py_repr(v)),
                format!("its verbs: {}", names.join(", ")),
            );
            with(&mut r, "arg", json!("verb"));
            return Err(r);
        }
    }
    let picked: Vec<&crate::verbs::Row> = rows
        .iter()
        .filter(|r| verb.is_none_or(|v| v == r.name))
        .collect();
    if part != Part::Schema {
        out.insert("tier".into(), json!(tier.as_str()));
        let verbs: Map<String, Value> = picked
            .iter()
            .map(|r| {
                let text = if tier == Tier::Detailed {
                    crate::verbs::detail(r)
                } else {
                    crate::verbs::line(r)
                };
                (r.name.to_string(), Value::String(text))
            })
            .collect();
        out.insert("verbs".into(), Value::Object(verbs));
        if let (None, Some(man)) = (verb, man) {
            out.insert("man".into(), json!(man));
        }
    }
    if part != Part::Docs {
        let schemas: Map<String, Value> = picked
            .iter()
            .map(|r| (r.name.to_string(), r.schema.clone()))
            .collect();
        out.insert("schemas".into(), Value::Object(schemas));
    }
    Ok(out)
}

/// The verb docs of an agent on a CONNECTED KERNEL (`<guid>|<agent id>`): THAT
/// kernel's own `reflect` of the agent, asked with `tier`, `verb` and `part`. This
/// kernel asks as itself over the peer's live routes, and the peer judges its leg.
/// An id of this kernel's own guid is answered here, as the operator reads it. A
/// qualified id naming no connected kernel is the unknown answer.
pub async fn remote_docs(
    kernel: &Arc<Kernel>,
    qualified: &str,
    tier: Option<Tier>,
    verb: Option<&str>,
    part: Option<Part>,
) -> Value {
    let Some(target) = kernel.qualified_target(qualified) else {
        return crate::refusal::unknown_agent(qualified);
    };
    let mut ask = json!({"type": "reflect"});
    if let Some(t) = tier {
        ask["tier"] = json!(t.as_str());
    }
    if let Some(v) = verb {
        ask["verb"] = json!(v);
    }
    if let Some(p) = part {
        ask["part"] = json!(p.as_str());
    }
    let Some(remote) = target.remote.as_ref() else {
        // this kernel's own guid — a LOCAL id, answered here
        return local_docs(kernel, &target, &ask);
    };
    crate::peers::ask_peer(remote, ask).await
}

/// The verb section of a local agent's reflect `ask`, read as the operator.
fn local_docs(kernel: &Arc<Kernel>, target: &Arc<Agent>, ask: &Value) -> Value {
    let args = match crate::declared::common::Call::parse(&target.id.0, ask) {
        Ok(crate::declared::common::Call::Reflect(a)) => a,
        Err(refused) => return refused,
    };
    let bundle = target
        .handler_module
        .as_deref()
        .and_then(|hm| kernel.bundles.get(hm));
    let dynamic = bundle
        .as_ref()
        .map(|b| b.dynamic_verbs(target))
        .unwrap_or_default();
    let lists = crate::reflect::verb_lists(target, bundle.as_deref());
    let rows = crate::verbs::rows(&lists, &dynamic);
    match section(&target.id.0, &args, &rows, bundle.as_ref().map(|b| b.man())) {
        Ok(m) => Value::Object(m),
        Err(refused) => refused,
    }
}

// ─── reflect subtree filters ────────────────────────────────────

/// Is `agent` `ancestor` or below it? The parent-chain walk the gate uses.
fn within(kernel: &Kernel, agent: &Agent, ancestor: &Agent) -> bool {
    if agent.id == ancestor.id {
        return true;
    }
    let mut seen = HashSet::new();
    let mut next = agent.parent_id.clone();
    while let Some(pid) = next {
        if pid == ancestor.id {
            return true;
        }
        if !seen.insert(pid.clone()) {
            return false;
        }
        next = kernel.agents.get(&pid).and_then(|a| a.parent_id.clone());
    }
    false
}

/// Check `show_subtrees` / `ignore_subtrees` of a reflect of `target` against the
/// tree — python `_docs.subtree_filter`. `Ok((show, ignore))`: the branch roots to
/// answer with (empty: the whole subtree of `target`) and the ids pruned to a
/// stub; `Err` a coded refusal when an id is not an agent in `target`'s tree. A shown id
/// under another shown id is not repeated. An id `sight` does not cover is
/// answered as a missing one.
pub(crate) fn subtree_filter(
    kernel: &Arc<Kernel>,
    sight: &crate::reflect::Sight,
    args: &Reflect,
    target: &Arc<Agent>,
) -> Result<(Vec<Arc<Agent>>, HashSet<String>), Value> {
    let named = [
        (
            "show_subtrees",
            args.show_subtrees.as_deref().unwrap_or(&[]),
        ),
        (
            "ignore_subtrees",
            args.ignore_subtrees.as_deref().unwrap_or(&[]),
        ),
    ];
    for (arg, ids) in named {
        for x in ids {
            let ok = kernel
                .agents
                .get(&crate::agent::AgentId(x.clone()))
                .is_some_and(|a| within(kernel, &a, target) && sight.sees(&a));
            if !ok {
                let mut r = refusal_hint(
                    BAD_ARG,
                    format!(
                        "reflect: {arg} names {} — not an agent in {}'s tree",
                        fantastic_bundle::py_repr(x),
                        target.id.0
                    ),
                    "reflect with tree='ids' lists the ids",
                );
                with(&mut r, "arg", json!(arg));
                return Err(r);
            }
        }
    }
    let shown: &[String] = named[0].1;
    let ignored: HashSet<String> = named[1].1.iter().cloned().collect();
    let mut seen = HashSet::new();
    let mut show = Vec::new();
    for x in shown {
        if !seen.insert(x.clone()) {
            continue;
        }
        let Some(a) = kernel
            .agents
            .get(&crate::agent::AgentId(x.clone()))
            .map(|e| Arc::clone(&e))
        else {
            continue;
        };
        let nested = shown.iter().any(|o| {
            o != x
                && kernel
                    .agents
                    .get(&crate::agent::AgentId(o.clone()))
                    .is_some_and(|anc| within(kernel, &a, &anc))
        });
        if !nested {
            show.push(a);
        }
    }
    Ok((show, ignored))
}

/// What an ignored branch leaves in the tree: its id and how many agents it
/// holds (itself included) that `sight` covers. Python `_docs.pruned_stub`.
#[must_use]
pub(crate) fn pruned_stub(agent: &Agent, sight: &crate::reflect::Sight) -> Value {
    json!({"id": agent.id.0, "pruned": crate::reflect::descendant_ids(agent, sight).len()})
}
