//! The writer subset check (grant model v9, rule 7, #754): an agent hands out only
//! what the lines WRITTEN in its own `can` give, capped by every ancestor whose tree
//! does not hold the line's target.
//!
//! - **Recipient.** An existing recipient needs `update can` from the writer. An
//!   agent being created needs nothing more: the create line writes its first `can`.
//! - **Held = written lines only.** Position never counts when handing out lines:
//!   every probe goes through [`Kernel::allowed_written`].
//! - **`$parent`** resolves to the recipient's parent. It names one node, so
//!   the writer's written lines on that node hold it.
//! - **`call` / `update` on a literal id** reach the target's whole tree, so
//!   only a TREE-WIDE hold counts ([`Basis::Wide`]): not the writer's
//!   `$parent` line, and a `create` line only with `who: *` and every bundle;
//!   each capping ancestor's hold likewise. Each verb or field must be held, a
//!   `*` as `*`.
//! - **`update`** includes calling every verb, so an `update` line (an empty
//!   field list included) also needs every verb held.
//! - **`create`**: each bundle × `who` must be creatable; the fields and forced
//!   values are compared with the CAPPED HOLD on the same target (the writer's
//!   create lines whose target's tree holds it, and those of every capping
//!   ancestor). A forced `can` inside `what` is checked here, once, as lines the
//!   writer hands out.
//! - Only lines added or widened against the stored `can` are checked; narrowing
//!   is always allowed.
//! - Every refusal on an id the writer holds nothing on is the unknown answer for
//!   that id, byte-identical to a missing id.
//!
//! The operator (origin `None`) is never checked: callers skip this module for it.

use super::line::{parse_can, Level, Line, Target, What};
use super::solver::{lines_of, Act, Basis, Slot};
use crate::agent::{Agent, AgentId};
use crate::kernel::Kernel;
use crate::lifecycle::{field_rule, matching_create_lines, what_selects, FieldRule};
use crate::send::gate::{sample_id, Held, PROBE};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Who receives the lines a writer hands out.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Recipient<'a> {
    /// An agent that exists: the writer must hold `update can` on it.
    Existing(&'a AgentId),
    /// An agent being created under `parent`: the create line writes its first
    /// `can`, so no `update can` is asked for; `$parent` is `parent`.
    Pending {
        /// The agent it is created under.
        parent: &'a AgentId,
    },
}

fn repr(s: &str) -> String {
    fantastic_bundle::py_repr(s)
}

fn agent(kernel: &Kernel, id: &AgentId) -> Option<Arc<Agent>> {
    kernel.agents.get(id).map(|e| Arc::clone(&e))
}

/// The refusal of a write about `id`: the unknown answer when the writer holds
/// nothing on `id` (a missing id included), else `not_allowed` naming what is
/// missing.
fn refuse(kernel: &Kernel, writer: &AgentId, id: &AgentId, why: impl FnOnce() -> String) -> Value {
    if kernel.held(writer, id) == Held::Nothing {
        return crate::refusal::unknown_agent(id.as_str());
    }
    crate::refusal::not_allowed(
        "can",
        why(),
        format!("ask whoever holds a wider line on {}", repr(id.as_str())),
    )
}

/// May `writer` hand `line` to `recipient`? `Err` is the refusal reply.
pub(crate) fn can_write(
    kernel: &Kernel,
    writer: &AgentId,
    recipient: Recipient<'_>,
    line: &Line,
) -> Result<(), Value> {
    if let Recipient::Existing(r) = recipient {
        may_write_can(kernel, writer, r)?;
    }
    hands_out(kernel, writer, recipient, line)
}

/// Rule 7 on an edit of `recipient`'s `can` from `old` to `new` by `writer`: the
/// writer holds `update can` on the recipient, and every line of `new` that is not
/// unchanged or narrowed against a line of `old` goes through [`can_write`]. Both
/// values are already of the legal shape.
pub(crate) fn check_can_edit(
    kernel: &Kernel,
    writer: &AgentId,
    recipient: &AgentId,
    old: &Value,
    new: &Value,
) -> Result<(), Value> {
    may_write_can(kernel, writer, recipient)?;
    let stored = lines_of(old);
    for line in lines_of(new) {
        if !stored.iter().any(|s| narrows(&line, s)) {
            hands_out(kernel, writer, Recipient::Existing(recipient), &line)?;
        }
    }
    Ok(())
}

/// Does `writer` hold `update can` on the existing agent `r` by its written lines
/// (capped)?
fn may_write_can(kernel: &Kernel, writer: &AgentId, r: &AgentId) -> Result<(), Value> {
    let Some(ra) = agent(kernel, r) else {
        return Err(crate::refusal::unknown_agent(r.as_str()));
    };
    if kernel.allowed_written(writer, &Act::Update("can".to_string()), &ra) {
        return Ok(());
    }
    Err(refuse(kernel, writer, r, || {
        format!("{} holds no update of can on {}", repr(writer.as_str()), repr(r.as_str()))
    }))
}

/// [`can_write`] without the recipient's `update can` check.
fn hands_out(
    kernel: &Kernel,
    writer: &AgentId,
    recipient: Recipient<'_>,
    line: &Line,
) -> Result<(), Value> {
    let t: AgentId = match (&line.target, recipient) {
        (Target::Id(t), _) => AgentId::from(t.as_str()),
        (Target::Parent, Recipient::Pending { parent }) => parent.clone(),
        (Target::Parent, Recipient::Existing(r)) => {
            match agent(kernel, r).and_then(|a| a.parent_id.clone()) {
                Some(p) => p,
                None => {
                    return Err(refuse(kernel, writer, r, || {
                        format!("can[$parent]: {} has no parent", repr(r.as_str()))
                    }));
                }
            }
        }
    };
    // a line on a missing id: the writer holds nothing there
    let Some(ta) = agent(kernel, &t) else {
        return Err(crate::refusal::unknown_agent(t.as_str()));
    };
    let lacks = |what: String| {
        refuse(kernel, writer, &t, || {
            format!(
                "{} holds no {what} on {} — a writer hands out only what its own can holds",
                repr(writer.as_str()),
                repr(t.as_str())
            )
        })
    };
    // a `$parent` line reaches one node; a line on a literal id its whole tree
    let basis = match line.target {
        Target::Parent => Basis::Written,
        Target::Id(_) => Basis::Wide,
    };
    let holds = |act: Act| kernel.capped_on(writer, &act, &ta, basis);
    let holds_all = |slot: Slot| kernel.allowed_all(writer, slot, &ta, basis);
    match &line.level {
        Level::Call(verbs) => {
            for v in verbs {
                let held = if v == "*" {
                    holds_all(Slot::Verb)
                } else {
                    holds(Act::Call(v.clone()))
                };
                if !held {
                    return Err(lacks(format!("call {}", repr(v))));
                }
            }
        }
        Level::Update(fields) => {
            // `update` includes `call` on every verb: held in writing too
            if !holds_all(Slot::Verb) {
                return Err(lacks(
                    "call '*' (an update line calls every verb)".to_string(),
                ));
            }
            for f in fields {
                let held = if f == "*" {
                    holds_all(Slot::Field)
                } else {
                    holds(Act::Update(f.clone()))
                };
                if !held {
                    return Err(lacks(format!("update {}", repr(f))));
                }
            }
        }
        Level::Create { who, what } => {
            let out: Vec<(String, Map<String, Value>)> = match what {
                What::All => {
                    let mut all = Map::new();
                    all.insert("*".to_string(), json!("*"));
                    vec![("*".to_string(), all)]
                }
                What::Bundles(m) => m.iter().map(|(b, f)| (b.clone(), f.clone())).collect(),
            };
            let holders = capped_holders(kernel, writer, &ta.id);
            for (b, fields) in &out {
                let sent = if b == "*" { PROBE } else { b.as_str() };
                for w in who {
                    let new_id = sample_id(w);
                    let act = Act::Create {
                        bundle: sent.to_string(),
                        new_id: new_id.clone(),
                    };
                    if !kernel.allowed_written(writer, &act, &ta) {
                        return Err(lacks(format!("create of {} for {}", repr(b), repr(w))));
                    }
                    for h in &holders {
                        let hold = matching_create_lines(kernel, h, &ta.id, sent, &new_id);
                        if let Err(why) = fields_within(&hold, sent, b, fields) {
                            return Err(lacks(why));
                        }
                    }
                }
                // a `can` forced in `what` is handed out by this writer: checked
                // once, here, against the writer's own lines
                if let Some(forced) = fields.get("can").filter(|v| v.as_str() != Some("*")) {
                    let lines = parse_can(forced).map_err(|why| {
                        let mut r = crate::refusal::refusal(
                            crate::refusal::INVALID_META,
                            format!("create.what[{}].can: {why}", repr(b)),
                        );
                        crate::refusal::with(&mut r, "arg", json!("can"));
                        r
                    })?;
                    // A forced `$parent` resolves to whichever node of ta's tree the
                    // child is made under (the create line reaches that whole tree),
                    // so it is checked as a line on ta: held tree-wide there.
                    for l in &lines {
                        let l = match l.target {
                            Target::Parent => Line {
                                target: Target::Id(ta.id.0.clone()),
                                level: l.level.clone(),
                            },
                            Target::Id(_) => l.clone(),
                        };
                        hands_out(kernel, writer, Recipient::Pending { parent: &ta.id }, &l)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The writer and each ancestor of it whose tree does not hold `t`: the agents
/// whose create lines make up the capped hold on `t`.
fn capped_holders(kernel: &Kernel, writer: &AgentId, t: &AgentId) -> Vec<AgentId> {
    let mut out = vec![writer.clone()];
    if let Some(w) = agent(kernel, writer) {
        out.extend(
            kernel
                .ancestor_chain(&w)
                .into_iter()
                .filter(|a| !kernel.in_tree(t, a)),
        );
    }
    out
}

/// Are the handed-out `fields` of bundle `b` (probed as `sent`) within one
/// holder's matching create lines `hold`?
///
/// - a field the hold forces must be carried with the same value (changing it,
///   freeing it or leaving it out is refused);
/// - a field the hold frees may be freed or forced to any value;
/// - a field the hold does not name is refused.
fn fields_within(
    hold: &[(Vec<String>, What)],
    sent: &str,
    b: &str,
    fields: &Map<String, Value>,
) -> Result<(), String> {
    for (_, hw) in hold {
        let What::Bundles(m) = hw else { continue };
        for hb in [sent, "*"] {
            let Some(hfs) = m.get(hb) else { continue };
            for f in hfs.keys().filter(|f| f.as_str() != "*") {
                let FieldRule::Forced(v) = field_rule(hw, sent, f) else {
                    continue;
                };
                if fields.get(f) != Some(&v) {
                    return Err(format!(
                        "create of {} with {} other than forced to {}",
                        repr(b),
                        repr(f),
                        fantastic_bundle::py_repr_value(&v)
                    ));
                }
            }
        }
    }
    for (f, given) in fields {
        let rules: Vec<FieldRule> = hold.iter().map(|(_, hw)| field_rule(hw, sent, f)).collect();
        let forced: Vec<&Value> = rules
            .iter()
            .filter_map(|r| match r {
                FieldRule::Forced(v) => Some(v),
                _ => None,
            })
            .collect();
        let ok = if forced.is_empty() {
            rules.contains(&FieldRule::Free)
        } else {
            given.as_str() != Some("*") && forced.iter().all(|v| *v == given)
        };
        if !ok {
            return Err(format!("create of {} with field {}", repr(b), repr(f)));
        }
    }
    Ok(())
}

/// Is `new` unchanged or narrowed against the stored line `old`: the same target
/// and level, whose verbs, fields, `who` entries and bundles contain `new`'s, with
/// the same forced values?
fn narrows(new: &Line, old: &Line) -> bool {
    if new.target != old.target {
        return false;
    }
    match (&new.level, &old.level) {
        (Level::Call(n), Level::Call(o)) | (Level::Update(n), Level::Update(o)) => {
            names_within(n, o)
        }
        (Level::Create { who: nw, what: nt }, Level::Create { who: ow, what: ot }) => {
            names_within(nw, ow) && what_within(nt, ot) && !forces_new_can(nt, ot)
        }
        _ => false,
    }
}

/// Does `n` force a `can` on some bundle that `o` does not force with the same
/// value? A forced `can` hands out lines and is checked when its create line is
/// written (Q23), so a line that adds or changes one never counts as narrowed,
/// even where `o` frees `can` or every field.
fn forces_new_can(n: &What, o: &What) -> bool {
    let What::Bundles(nm) = n else {
        return false;
    };
    nm.keys().any(|b| match field_rule(n, b, "can") {
        FieldRule::Forced(v) => field_rule(o, b, "can") != FieldRule::Forced(v),
        _ => false,
    })
}

/// Is every name of `n` in `o` (a `*` in `o` holds every name)?
fn names_within(n: &[String], o: &[String]) -> bool {
    o.iter().any(|x| x == "*") || n.iter().all(|x| o.contains(x))
}

/// Does `o` select every bundle `n` does, freeing at least what `n` frees and
/// forcing the same values?
fn what_within(n: &What, o: &What) -> bool {
    let (nm, om) = match (n, o) {
        (_, What::All) => return true,
        (What::All, What::Bundles(_)) => return false,
        (What::Bundles(nm), What::Bundles(om)) => (nm, om),
    };
    nm.keys().all(|b| {
        if !what_selects(o, b) {
            return false;
        }
        let mut names: Vec<&str> = vec!["*"];
        for m in [nm, om] {
            for k in [b.as_str(), "*"] {
                if let Some(fs) = m.get(k) {
                    names.extend(fs.keys().map(String::as_str));
                }
            }
        }
        names.iter().all(|f| {
            match (field_rule(n, b, f), field_rule(o, b, f)) {
                (_, FieldRule::Free) | (FieldRule::Unnamed, FieldRule::Unnamed) => true,
                (FieldRule::Forced(a), FieldRule::Forced(c)) => a == c,
                _ => false,
            }
        })
    })
}
