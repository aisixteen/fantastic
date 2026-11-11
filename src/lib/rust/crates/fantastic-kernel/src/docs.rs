//! HOW TO CALL an agent — the `docs` kernel verb, and the reflect subtree
//! filters (python `kernel/_docs.py`).
//!
//! A reflect says what an agent IS: id, sentence, state,
//! grants, tree. How to call it — verb signatures, emits, cancel, notes — is
//! `docs`, pulled per BUNDLE and rendered from its declared verbs, in two tiers:
//! `oneliners` (default) — each verb's signature line; `detailed` — the whole
//! text. Without a `verb` filter the reply also carries the bundle's one short
//! `man` text — the only prose a bundle has; everything else is rendered from the
//! declarations.

use crate::agent::Agent;
use crate::declared::common::Reflect;
use crate::declared::kernel::Docs;
use crate::declared::Tier;
use crate::kernel::Kernel;
use crate::refusal::{refusal_hint, with, BAD_ARG};
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

/// The `docs` reply `{of, bundle, tier, verbs, man?}` — `man` only when no `verb`
/// is asked for. Python `_docs.render`.
#[must_use]
pub fn render(
    of: &str,
    bundle: Value,
    tier: Tier,
    verb: Option<&str>,
    table: &[Entry],
    man: Option<&str>,
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
        let text = if tier == Tier::Detailed { detail } else { line };
        verbs.insert(n.clone(), Value::String(text.clone()));
    }
    let mut out = Map::new();
    out.insert("of".into(), json!(of));
    out.insert("bundle".into(), bundle);
    out.insert("tier".into(), json!(tier.as_str()));
    out.insert("verbs".into(), Value::Object(verbs));
    if let (None, Some(man)) = (verb, man) {
        out.insert("man".into(), json!(man));
    }
    Value::Object(out)
}

/// `docs` of an agent on a CONNECTED KERNEL, `of: <guid>|<agent id>` — python
/// `Agent._remote_docs`. The question goes to THAT kernel's own
/// `docs`, and the reply names the qualified id. The asker's own `docs` grant on THIS
/// root is what the gate checked; this kernel then asks as itself over the peer's
/// live routes, and the peer judges its leg. With verbs out of reflect, this is
/// how a model learns a page agent's verbs at all.
pub async fn remote_docs_reply(kernel: &Arc<Kernel>, args: &Docs) -> Value {
    let of = args.of.as_str();
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
    let local = Docs {
        of: agent_id.to_string(),
        tier: args.tier,
        verb: args.verb.clone(),
    };
    let Some(remote) = target.remote.as_ref() else {
        // this kernel's own guid — a LOCAL id, answered here
        return docs_reply(kernel, &local);
    };
    let mut ask = json!({"type": "docs", "of": agent_id});
    if let Some(v) = &args.verb {
        ask["verb"] = json!(v);
    }
    if let Some(t) = args.tier {
        ask["tier"] = json!(t.as_str());
    }
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
pub fn docs_reply(kernel: &Arc<Kernel>, args: &Docs) -> Value {
    let of = args.of.as_str();
    let tier = args.tier.unwrap_or(Tier::Oneliners);
    let verb = args.verb.as_deref();
    if of == "kernel" {
        return render(
            of,
            json!("kernel"),
            tier,
            verb,
            &entries_of_decls(&[crate::declared::kernel::VERBS], &[]),
            None,
        );
    }
    let agent = kernel
        .agents
        .get(&crate::agent::AgentId(of.to_string()))
        .map(|e| Arc::clone(e.value()));
    let module = if let Some(a) = &agent {
        match a.handler_module.clone() {
            Some(hm) => hm,
            None => return render(of, Value::Null, tier, verb, &[], None),
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
        return render(of, json!(module), tier, verb, &[], None);
    };
    // The bundle's declarations, with the verbs every agent answers.
    // The agent's own dynamic verbs too (a bundle addressed by name has none).
    let dynamic = agent
        .as_deref()
        .map(|a| bundle.dynamic_verbs(a))
        .unwrap_or_default();
    let table = entries_of_decls(&[bundle.verbs(), crate::declared::common::VERBS], &dynamic);
    render(of, json!(module), tier, verb, &table, Some(bundle.man()))
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
/// under another shown id is not repeated.
pub fn subtree_filter(
    kernel: &Arc<Kernel>,
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
/// holds (itself included). Python `_docs.pruned_stub`.
#[must_use]
pub fn pruned_stub(agent: &Agent) -> Value {
    json!({"id": agent.id.0, "pruned": crate::reflect::descendant_ids(agent).len()})
}
