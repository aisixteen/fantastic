//! SECRET MARKS on a send item — python `kernel/_secret.py`.
//!
//! An item may carry `secret: ["<dotted path>", ...]` — paths INTO its payload
//! (`"fields.api_key"`). The handler gets the payload as sent; everything the
//! kernel keeps or shows of the call — the call row, the state stream's
//! `payload`/`summary`, the inbox mirrors to watchers — carries it AS SHOWN:
//! each marked value replaced by [`SHOWN`]. A path naming nothing is skipped;
//! `type` cannot be marked (the verb is routing, not data). A handler reads the
//! marks as [`current_secret`] and marks its own onward send the same way.

use std::sync::Arc;

use serde_json::{json, Value};

/// What a marked value is shown as.
pub const SHOWN: &str = "[secret]";

tokio::task_local! {
    /// The marks of the item the running dispatch serves.
    pub(crate) static CURRENT_SECRET: Arc<Vec<String>>;
}

/// The `secret` marks of the item this dispatch serves — dotted paths into its
/// payload. Empty outside a marked dispatch.
#[must_use]
pub fn current_secret() -> Vec<String> {
    CURRENT_SECRET
        .try_with(|m| m.as_ref().clone())
        .unwrap_or_default()
}

/// The item's marks, or a `bad_arg` refusal for a malformed field. Absent ⇒ none.
///
/// # Errors
/// The refusal, as python words it.
pub fn check(raw: Option<&Value>) -> Result<Vec<String>, Value> {
    let Some(raw) = raw.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let marks: Option<Vec<String>> = raw.as_array().and_then(|a| {
        a.iter()
            .map(|m| {
                m.as_str()
                    .filter(|s| !s.is_empty() && !s.split('.').any(str::is_empty))
                    .map(String::from)
            })
            .collect()
    });
    let Some(marks) = marks else {
        let mut r = crate::refusal::refusal(
            crate::refusal::BAD_ARG,
            format!(
                "send: secret must be a list of dotted paths into the payload \
                 (e.g. [\"fields.api_key\"]), got {}",
                fantastic_bundle::py_repr_value(raw)
            ),
        );
        crate::refusal::with(&mut r, "arg", json!("secret"));
        return Err(r);
    };
    if marks.iter().any(|m| m == "type") {
        let mut r = crate::refusal::refusal(
            crate::refusal::BAD_ARG,
            "send: secret may not mark `type` — the verb is not data",
        );
        crate::refusal::with(&mut r, "arg", json!("secret"));
        return Err(r);
    }
    Ok(marks)
}

/// `payload` with every marked value replaced by [`SHOWN`]; no marks ⇒ a copy.
#[must_use]
pub fn shown(payload: &Value, marks: &[String]) -> Value {
    let mut out = payload.clone();
    for mark in marks {
        let mut steps: Vec<&str> = mark.split('.').collect();
        let Some(leaf) = steps.pop() else { continue };
        let mut node = Some(&mut out);
        for key in steps {
            node = node.and_then(|n| n.get_mut(key));
        }
        if let Some(Value::Object(m)) = node {
            if let Some(v) = m.get_mut(leaf) {
                *v = Value::from(SHOWN);
            }
        }
    }
    out
}
