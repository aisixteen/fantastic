//! One `can` line: a target and the one level held on it (grant model v9, #754).
//!
//! A record's `can` is `{target: {call: [verbs]} | {update: [fields]} |
//! {create: {who: [ids], what: "*" | {bundle: {field: value}}}}}`. A target is a
//! literal local agent id or `$parent`, and holds exactly one level. `*` means
//! everything in any slot (verbs, fields, ids, bundles); a target is never a
//! wildcard. Peer targets (`<guid>|<agent>`, `<guid>|*`) are parked.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The agent a line is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A literal local agent id.
    Id(String),
    /// The holder's parent (`$parent`); `call` level only.
    Parent,
}

/// The bundles a `create` line selects.
#[derive(Debug, Clone, PartialEq)]
pub enum What {
    /// `what: "*"` — every bundle, every field.
    All,
    /// `{bundle | "*": {field | "*": value | "*"}}`. A bundle key is the bundle's
    /// registered name as `handler_module` shows it (a meta bundle by its own name);
    /// `"*"` as a bundle key covers every bundle, as a field key every field.
    Bundles(BTreeMap<String, Map<String, Value>>),
}

/// The one level a line holds on its target. `update` includes `call`, and
/// `create` includes `update`.
#[derive(Debug, Clone, PartialEq)]
pub enum Level {
    /// The verbs it may call on the target's tree.
    Call(Vec<String>),
    /// The fields it may change on the target's tree.
    Update(Vec<String>),
    /// Creating directly under the target: `who` ids (at most one trailing `*`
    /// each) and the bundles `what` allows.
    Create {
        /// The ids it may create, each with at most one trailing `*`.
        who: Vec<String>,
        /// The bundles, and the fields of each, it may create.
        what: What,
    },
}

/// One `can` line.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// The agent the line is about.
    pub target: Target,
    /// The one level held on it.
    pub level: Level,
}

/// Does `id` match the id pattern of `CreateAgent.id`?
fn is_agent_id(id: &str) -> bool {
    crate::verbs::check_agent_id("can", "id", id).is_ok()
}

/// A verb, field or bundle name: no `\` and no control character (a `who` goes into
/// a Cedar `like` pattern, and the glue samples ids with `\u{1}`).
fn bad_name(name: &str) -> bool {
    name.chars().any(|c| c == '\\' || c.is_control())
}

fn repr(s: &str) -> String {
    fantastic_bundle::py_repr(s)
}

/// A slot that must be a list of names.
fn names(target: &str, slot: &str, v: &Value) -> Result<Vec<String>, String> {
    let Some(items) = v.as_array() else {
        return Err(format!(
            "can[{}].{slot} is not a list of strings",
            repr(target)
        ));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(s) = item.as_str() else {
            return Err(format!(
                "can[{}].{slot} is not a list of strings",
                repr(target)
            ));
        };
        out.push(s.to_string());
    }
    Ok(out)
}

/// The `who` slot of a `create`: ids, each with at most one trailing `*`; `"*"` is
/// every id.
fn who(target: &str, v: &Value) -> Result<Vec<String>, String> {
    let ids = names(target, "create.who", v)?;
    for id in &ids {
        let stars = id.matches('*').count();
        let stem = id.strip_suffix('*').unwrap_or(id);
        let ok = match stars {
            0 => is_agent_id(id),
            1 if id.ends_with('*') => stem.is_empty() || is_agent_id(stem),
            _ => false,
        };
        if !ok {
            return Err(format!(
                "can[{}].create.who[{}] is not an agent id with at most one trailing *",
                repr(target),
                repr(id)
            ));
        }
    }
    Ok(ids)
}

/// The `what` slot of a `create`: `"*"` or `{bundle | "*": {field | "*": value | "*"}}`.
fn what(target: &str, v: &Value) -> Result<What, String> {
    if v.as_str() == Some("*") {
        return Ok(What::All);
    }
    let Some(bundles) = v.as_object() else {
        return Err(format!(
            "can[{}].create.what is not \"*\" or a map of bundles",
            repr(target)
        ));
    };
    let mut out = BTreeMap::new();
    for (bundle, fields) in bundles {
        if bundle.is_empty() || bad_name(bundle) {
            return Err(format!(
                "can[{}].create.what[{}] is not a bundle name",
                repr(target),
                repr(bundle)
            ));
        }
        let Some(fields) = fields.as_object() else {
            return Err(format!(
                "can[{}].create.what[{}] is not a map of fields",
                repr(target),
                repr(bundle)
            ));
        };
        for field in fields.keys() {
            if field.is_empty() || bad_name(field) {
                return Err(format!(
                    "can[{}].create.what[{}][{}] is not a field name",
                    repr(target),
                    repr(bundle),
                    repr(field)
                ));
            }
        }
        out.insert(bundle.clone(), fields.clone());
    }
    Ok(What::Bundles(out))
}

/// The one level key of a target's entry.
fn level(target: &str, v: &Value) -> Result<Level, String> {
    let Some(entry) = v.as_object() else {
        return Err(format!(
            "can[{}] is not a map holding one of call, update, create",
            repr(target)
        ));
    };
    if entry.len() != 1 {
        return Err(format!(
            "can[{}] holds {} levels — a target holds exactly one of call, update, create",
            repr(target),
            entry.len()
        ));
    }
    let (key, slot) = entry.iter().next().expect("one entry");
    let lvl = match key.as_str() {
        "call" => Level::Call(names(target, "call", slot)?),
        "update" => Level::Update(names(target, "update", slot)?),
        "create" => {
            let Some(c) = slot.as_object() else {
                return Err(format!(
                    "can[{}].create is not a map of who and what",
                    repr(target)
                ));
            };
            if let Some(k) = c.keys().find(|k| *k != "who" && *k != "what") {
                return Err(format!(
                    "can[{}].create[{}] is not a create key — legal keys: who, what",
                    repr(target),
                    repr(k)
                ));
            }
            let (Some(w), Some(t)) = (c.get("who"), c.get("what")) else {
                return Err(format!(
                    "can[{}].create needs both who and what",
                    repr(target)
                ));
            };
            Level::Create {
                who: who(target, w)?,
                what: what(target, t)?,
            }
        }
        other => {
            return Err(format!(
                "can[{}][{}] is not a level — legal levels: call, update, create",
                repr(target),
                repr(other)
            ));
        }
    };
    if let Level::Call(ref list) | Level::Update(ref list) = lvl {
        if let Some(bad) = list.iter().find(|n| bad_name(n)) {
            return Err(format!(
                "can[{}].{key}[{}] holds a \\ or a control character",
                repr(target),
                repr(bad)
            ));
        }
    }
    Ok(lvl)
}

/// The target key of one line.
fn target(key: &str) -> Result<Target, String> {
    if key == "$parent" {
        return Ok(Target::Parent);
    }
    if key.contains('|') {
        return Err(format!(
            "can[{}] is a peer target — peer lines are not part of the grant model yet",
            repr(key)
        ));
    }
    if key.is_empty() || key == "*" || key.starts_with('$') || !is_agent_id(key) {
        return Err(format!(
            "can[{}] is not a target — a target is an agent id or $parent, never a wildcard",
            repr(key)
        ));
    }
    Ok(Target::Id(key.to_string()))
}

/// Parse a record's `can` into its lines, refusing any line outside the legal
/// shape.
///
/// # Errors
/// One line naming the first part of `v` that is not legal.
pub fn parse_can(v: &Value) -> Result<Vec<Line>, String> {
    let Some(map) = v.as_object() else {
        return Err("can is not a map of targets".to_string());
    };
    let mut lines = Vec::with_capacity(map.len());
    for (key, entry) in map {
        let target = target(key)?;
        let level = level(key, entry)?;
        if target == Target::Parent && !matches!(level, Level::Call(_)) {
            return Err(format!(
                "can[{}] holds update or create — $parent takes call only",
                repr(key)
            ));
        }
        lines.push(Line { target, level });
    }
    Ok(lines)
}
