//! SECRET MARKS on a send item — python `kernel/_secret.py`.
//!
//! An item may carry `secret: ["<dotted path>", ...]` — paths INTO its payload
//! (`"fields.api_key"`). The handler gets the payload as sent; everything the
//! kernel keeps or shows of the call — the call row, the state stream's
//! `payload`/`summary`, the inbox mirrors to watchers — carries it AS SHOWN:
//! each marked value replaced by [`SHOWN`]. A path naming nothing is skipped;
//! `type` cannot be marked (the verb is routing, not data). A handler reads the
//! marks as [`current_secret`] and marks its own onward send the same way.
//! A BUNDLE may name such paths too — of a call (`Bundle::payload_secret`, added
//! to the item's marks) and of its reply (`Bundle::reply_secret`, carried as
//! `Outcome::secret`, shown by `crate::ledger::stored_outcome`).

use std::sync::{Arc, Mutex};

use serde_json::Value;

/// What a marked value is shown as.
pub const SHOWN: &str = "[secret]";

tokio::task_local! {
    /// The marks of the item the running dispatch serves.
    pub(crate) static CURRENT_SECRET: Arc<Vec<String>>;
}

/// Where a running dispatch collects the reply marks a handler notes.
pub(crate) type ReplySink = Arc<Mutex<Vec<String>>>;

tokio::task_local! {
    /// The reply marks the running dispatch's handler has noted so far.
    pub(crate) static REPLY_SECRET: ReplySink;
}

/// Note reply marks — dotted paths into THIS dispatch's reply — for a reply the
/// handler got from elsewhere (a connected kernel's store): the kernel records
/// the reply as [`SHOWN`] at those paths exactly as for `Bundle::reply_secret`,
/// while the caller still gets it whole. A no-op outside a dispatch.
pub fn note_reply_secret(marks: impl IntoIterator<Item = String>) {
    let _ = REPLY_SECRET.try_with(|sink| {
        if let Ok(mut all) = sink.lock() {
            all.extend(marks);
        }
    });
}

/// The well-formed marks among `noted` (non-empty steps, never `type`), each
/// once, after `base`.
pub(crate) fn well_formed(mut base: Vec<String>, noted: &[String]) -> Vec<String> {
    for m in noted {
        if m != "type" && !m.is_empty() && !m.split('.').any(str::is_empty) && !base.contains(m) {
            base.push(m.clone());
        }
    }
    base
}

/// The `secret` marks of the item this dispatch serves — dotted paths into its
/// payload. Empty outside a marked dispatch.
#[must_use]
pub fn current_secret() -> Vec<String> {
    CURRENT_SECRET
        .try_with(|m| m.as_ref().clone())
        .unwrap_or_default()
}

/// The item's marks, or a `bad_arg` refusal for a mark that is not a dotted path
/// into the payload or that marks `type`.
///
/// # Errors
/// The refusal.
pub fn check(marks: &[String]) -> Result<Vec<String>, Value> {
    if let Some(m) = marks.iter().find(|m| m.is_empty() || m.split('.').any(str::is_empty)) {
        return Err(crate::refusal::bad(
            "send",
            "secret",
            &format!(
                "a list of dotted paths into the payload, got {}",
                fantastic_bundle::py_repr(m)
            ),
        ));
    }
    if marks.iter().any(|m| m == "type") {
        return Err(crate::refusal::bad(
            "send",
            "secret",
            "a list of paths that leave out `type` (the verb is not data)",
        ));
    }
    Ok(marks.to_vec())
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
