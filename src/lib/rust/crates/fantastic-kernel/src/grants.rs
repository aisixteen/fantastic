//! Grant maps — the one rule for merging two of them.
//!
//! A grant map is the `callable_by` / `can_call` field of a record: `{<agent id>:
//! "*" | [<verb>, ...]}`.

use serde_json::{Map, Value};

/// Merge grant map `from` into `into`: a key `into` lacks is added; a key both
/// have becomes the wider of the two (`"*"` wins; two verb lists are unioned).
/// Nothing in `into` is removed or narrowed.
pub fn union(into: &mut Map<String, Value>, from: &Map<String, Value>) {
    for (k, v) in from {
        match into.get_mut(k) {
            None => {
                into.insert(k.clone(), v.clone());
            }
            Some(have) => {
                if have.as_str() == Some("*") {
                    continue;
                }
                match (have.as_array_mut(), v) {
                    (_, Value::String(s)) if s == "*" => *have = v.clone(),
                    (Some(a), Value::Array(b)) => {
                        for verb in b {
                            if !a.contains(verb) {
                                a.push(verb.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "grants/tests.rs"]
mod tests;
