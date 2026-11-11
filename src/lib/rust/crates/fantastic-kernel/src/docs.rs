//! HOW TO CALL an agent — the `docs` kernel verb, and the reflect subtree
//! filters (python `kernel/_docs.py`).
//!
//! A reflect says what an agent IS: id, sentence, state,
//! grants, tree. How to call it — verb signatures, emits, cancel, notes — is
//! `docs`, pulled per BUNDLE and rendered from its declared verbs, in two tiers:
//! `oneliners` (default) — each verb's signature line; `detailed` — the whole
//! text, and without a `verb` filter the bundle's emits / cancel / notes.

use crate::agent::Agent;
use crate::kernel::Kernel;
use crate::refusal::{refusal, refusal_hint, with, BAD_ARG};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;

/// One row of a docs answer: the verb, its ONE-LINE text and its DETAILED text.
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

/// The `docs` reply `{of, bundle, tier, verbs, emits?, cancel?, notes?}` —
/// extras only on a detailed reply with no `verb`. Python `_docs.render`.
#[must_use]
pub fn render(
    of: &str,
    bundle: Value,
    tier: &str,
    verb: Option<&str>,
    table: &[Entry],
    extras: &[(&str, Option<Value>)],
) -> Value {
    if let Some(v) = verb {
        if !table.iter().any(|(n, _, _)| n == v) {
            let names: Vec<&str> = table.iter().map(|(n, _, _)| n.as_str()).collect();
            let mut r = refusal_hint(
                BAD_ARG,
                format!("docs: {of} has no verb {}", fantastic_bundle::py_repr(v)),
                format!("its verbs: {}", names.join(", ")),
            );
            with(&mut r, "arg", json!("verb"));
            return r;
        }
    }
    let mut verbs = Map::new();
    for (n, line, detail) in table {
        if verb.is_some_and(|v| v != n) {
            continue;
        }
        let text = if tier == "detailed" { detail } else { line };
        verbs.insert(n.clone(), Value::String(text.clone()));
    }
    let mut out = Map::new();
    out.insert("of".into(), json!(of));
    out.insert("bundle".into(), bundle);
    out.insert("tier".into(), json!(tier));
    out.insert("verbs".into(), Value::Object(verbs));
    if tier == "detailed" && verb.is_none() {
        for (k, v) in extras {
            if let Some(v) = v.as_ref().filter(|v| !v.is_null()) {
                out.insert((*k).to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// `docs` of an agent on a CONNECTED KERNEL, `of: <guid>|<agent id>` — python
/// `Agent._remote_docs`. The question goes to THAT kernel's own
/// `docs`, and the reply names the qualified id. The asker's own `docs` grant on THIS
/// root is what the gate checked; this kernel then asks as itself over the peer's
/// live routes, and the peer judges its leg. With verbs out of reflect, this is
/// how a model learns a page agent's verbs at all.
pub async fn remote_docs_reply(kernel: &Arc<Kernel>, of: &str, payload: &Value) -> Value {
    let (guid, agent_id) = of.split_once('|').unwrap_or(("", ""));
    let target = if !guid.is_empty() && !agent_id.is_empty() && !agent_id.contains('|') {
        kernel.qualified_target(&format!("{guid}|kernel"))
    } else {
        None
    };
    let Some(target) = target else {
        let mut r = refusal_hint(
            BAD_ARG,
            format!(
                "docs: {} names no agent, bundle or 'kernel'",
                fantastic_bundle::py_repr(of)
            ),
            "a qualified id names an agent on a CONNECTED kernel (`<guid>|<agent id>`, \
             as a reflect tree lists it)",
        );
        with(&mut r, "arg", json!("of"));
        return r;
    };
    let mut ask = json!({"type": "docs", "of": agent_id});
    for k in ["verb", "tier"] {
        if let Some(v) = payload.get(k) {
            ask[k] = v.clone();
        }
    }
    let Some(remote) = target.remote.as_ref() else {
        // this kernel's own guid — a LOCAL id, answered here
        return docs_reply(kernel, &ask);
    };
    let (mut out, _) = crate::peers::call_peer(remote, ask, None).await;
    if let Some(o) = out.as_object_mut() {
        if o.contains_key("of") {
            o.insert("of".to_string(), json!(of));
        }
    }
    out
}

/// `docs {of, tier?, verb?}` — python `_verb_docs`. `of` is `kernel` (the kernel
/// verbs), an agent id (the bundle it runs), or a bundle's handler module,
/// readable before any agent of it exists.
#[must_use]
pub fn docs_reply(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    let of = match payload.get("of").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s,
        _ => {
            return crate::refusal::missing_hint(
                "docs",
                "of",
                "an agent id, a bundle (e.g. 'exec_bridge.tools') or 'kernel'",
            )
        }
    };
    let tier = match payload.get("tier") {
        None => "oneliners",
        Some(Value::String(t)) if t == "oneliners" || t == "detailed" => t.as_str(),
        _ => return crate::refusal::bad("docs", "tier", "'oneliners' or 'detailed'"),
    };
    let verb = match payload.get("verb") {
        None | Some(Value::Null) => None,
        Some(Value::String(v)) if !v.is_empty() => Some(v.as_str()),
        _ => return crate::refusal::bad("docs", "verb", "a verb name"),
    };
    if of == "kernel" {
        return render(
            of,
            json!("kernel"),
            tier,
            verb,
            &entries_of_decls(&[crate::declared::kernel::VERBS], &[]),
            &[],
        );
    }
    let agent = kernel
        .agents
        .get(&crate::agent::AgentId(of.to_string()))
        .map(|e| Arc::clone(e.value()));
    let module = if let Some(a) = &agent {
        match a.handler_module.clone() {
            Some(hm) => hm,
            None => return render(of, Value::Null, tier, verb, &[], &[]),
        }
    } else if kernel.bundles.is_folder(of) {
        // a FOLDER BUNDLE by its name — its handler_module
        of.to_string()
    } else if let Some((hm, _)) = kernel
        .bundles
        .catalog()
        // A BUNDLE by either name the kernel lists it under: `bundles='ids'` lists
        // the NAME (`file_bridge`), an agent record carries the handler_module
        // (`file_bridge.tools`) — a model asks for the listed name.
        .find(|(hm, b)| *hm == of || b.name() == of)
    {
        hm.to_string()
    } else {
        let mut r = refusal_hint(
            BAD_ARG,
            format!(
                "docs: {} names no agent, bundle or 'kernel'",
                fantastic_bundle::py_repr(of)
            ),
            "an agent id from a reflect tree, a bundle's name or handler_module \
             (reflect bundles='all' lists both), or 'kernel'",
        );
        with(&mut r, "arg", json!("of"));
        return r;
    };
    let Some(bundle) = kernel.bundles.get(&module) else {
        return render(of, json!(module), tier, verb, &[], &[]);
    };
    // The bundle's declarations, with the verbs every agent answers.
    // The agent's own dynamic verbs too (a bundle addressed by name has none).
    let dynamic = agent
        .as_deref()
        .map(|a| bundle.dynamic_verbs(a))
        .unwrap_or_default();
    let table = entries_of_decls(&[bundle.verbs(), crate::declared::common::VERBS], &dynamic);
    render(
        of,
        json!(module),
        tier,
        verb,
        &table,
        &[
            ("emits", bundle.emits_doc()),
            ("cancel", bundle.cancel_doc()),
            ("notes", bundle.notes_doc()),
        ],
    )
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

/// Validate `show_subtrees` / `ignore_subtrees` on a reflect of `target` —
/// python `_docs.subtree_filter`. `Ok((show, ignore))`: the branch roots to
/// answer with (empty: the whole subtree of `target`) and the ids pruned to a
/// stub; `Err` a coded refusal. A shown id under another shown id is not repeated.
pub fn subtree_filter(
    kernel: &Arc<Kernel>,
    payload: &Value,
    target: &Arc<Agent>,
) -> Result<(Vec<Arc<Agent>>, HashSet<String>), Value> {
    let mut lists: Vec<Vec<String>> = Vec::new();
    for arg in ["show_subtrees", "ignore_subtrees"] {
        match payload.get(arg) {
            None | Some(Value::Null) => lists.push(Vec::new()),
            Some(Value::Array(items))
                if items
                    .iter()
                    .all(|x| x.as_str().is_some_and(|s| !s.is_empty())) =>
            {
                let ids: Vec<String> = items
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect();
                for x in &ids {
                    let ok = kernel
                        .agents
                        .get(&crate::agent::AgentId(x.clone()))
                        .is_some_and(|a| within(kernel, &a, target));
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
                lists.push(ids);
            }
            Some(_) => {
                let mut r = refusal(
                    BAD_ARG,
                    format!("reflect: {arg} must be a list of agent ids"),
                );
                with(&mut r, "arg", json!(arg));
                return Err(r);
            }
        }
    }
    let shown = &lists[0];
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
    Ok((show, lists[1].iter().cloned().collect()))
}

/// What an ignored branch leaves in the tree: its id and how many agents it
/// holds (itself included). Python `_docs.pruned_stub`.
#[must_use]
pub fn pruned_stub(agent: &Agent) -> Value {
    json!({"id": agent.id.0, "pruned": crate::reflect::descendant_ids(agent).len()})
}
