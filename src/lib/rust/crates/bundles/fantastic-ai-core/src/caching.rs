//! The record's `caching` mode, and the cache flags it asks ai-core to set.
//!
//! `caching` picks how the model view is kept:
//! - `none`: the stored rows, in order. Append-only, no autofold, no marks.
//! - `provider_cache` (the default): append-only, AUTOFOLDED (a thought, a cut answer
//!   and a large or recalled reply show as an anchor, never stored), plus the cache
//!   marks the agent places on four messages: its own system message, the PRIMER END
//!   (the row `cache_primer_until` names), the LAST FOLD (the last fold or clear row in
//!   the view), and the TAIL: the row before the first row that will fold, else the
//!   last row. A backend that cannot cache sends no marks.
//!
//! ai-core sets ONE flag, [`CACHE_MARK`], on each message it wants marked ([`mark`]).
//! The backend forwards it and reads no mode.

use fantastic_bundle::py_repr_value;
use serde_json::{json, Map, Value};

/// The legal values of `caching`, in the order the schema lists them.
pub const MODES: [&str; 2] = ["none", "provider_cache"];
/// The mode a record without `caching` runs in.
pub const DEFAULT_MODE: &str = "provider_cache";
/// The one message flag the agent hands a backend: put a cache mark after this message.
pub const CACHE_MARK: &str = "cache_mark";

/// The `caching` field's hint.
pub const CACHING_HINT: &str = "how the conversation is kept in the model's view: \
     provider_cache (default) = every stored row, in order, autofolded, with the \
     provider's cache marks where the backend places them (the end of the primer row \
     named by cache_primer_until, the end of the last fold, and the end of the row \
     before the first row that will fold); none = every stored row, in order, no \
     autofold, no marks";

/// The `cache_primer_until` field's hint.
pub const CACHE_PRIMER_HINT: &str = "a row id: the row that ends the stable prefix. The \
     primer and every row before it are pinned — context_fold will not fold a range that \
     starts at or before it, in any mode. The provider is told where the prefix ends (the \
     end of that row's text) only under caching=provider_cache; absent, nothing is pinned \
     or marked";

/// The conversation's caching mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caching {
    /// Append-only: the stored rows, in order.
    None,
    /// Append-only, autofolded, with the provider's cache markers.
    ProviderCache,
}

impl Caching {
    /// The record's own spelling of the mode.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ProviderCache => "provider_cache",
        }
    }

    /// Whether the agent places cache marks.
    #[must_use]
    pub fn marks(self) -> bool {
        self == Self::ProviderCache
    }

    /// Whether the view is autofolded: anchors for thoughts, cuts and large replies.
    #[must_use]
    pub fn folds(self) -> bool {
        self != Self::None
    }
}

/// The record's `caching` mode. Absent or null is the default, `provider_cache`. Any other value is an
/// error — the write refuses it, so only a hand-edited record reaches here, and that
/// fails the turn rather than running a mode nobody chose.
///
/// # Errors
/// The record's `caching` is not one of [`MODES`]; the message names the value.
pub fn mode_of(meta: &Map<String, Value>) -> Result<Caching, String> {
    let raw = match meta.get("caching") {
        None | Some(Value::Null) => return Ok(Caching::ProviderCache),
        Some(v) => v,
    };
    match raw.as_str() {
        Some("none") => Ok(Caching::None),
        Some("provider_cache") => Ok(Caching::ProviderCache),
        _ => Err(format!(
            "caching={} is not one of the legal values: {}",
            py_repr_value(raw),
            MODES.join(", ")
        )),
    }
}

/// The record's `cache_primer_until`: a row id, or none. A value that is not a
/// positive int is an error, naming it.
///
/// # Errors
/// The record names a `cache_primer_until` that is not a row id.
pub fn primer_of(meta: &Map<String, Value>) -> Result<Option<i64>, String> {
    match meta.get("cache_primer_until") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_i64().filter(|n| *n > 0).map(Some).ok_or_else(|| {
            format!(
                "cache_primer_until={} must be a row id (a positive int)",
                py_repr_value(v)
            )
        }),
    }
}

/// The mode and the primer, read together off the agent's record.
///
/// # Errors
/// Either field is malformed; see [`mode_of`] and [`primer_of`].
pub fn settings(meta: &Map<String, Value>) -> Result<(Caching, Option<i64>), String> {
    Ok((mode_of(meta)?, primer_of(meta)?))
}

/// Set [`CACHE_MARK`] on the messages the cache marks land on, in the list it is
/// handed: the system message when it opens the list, the message of the row `primer`
/// names (none when that row is not in the view), the LAST message of a fold or clear
/// row, and the tail: the message before the first user message after the last
/// assistant one that will fold (a row that folds never carries the mark), else the
/// last message; the tail is never the system message. A message hit by several
/// reasons carries one flag. The flag is on the message's own text, so a backend can
/// cut its block there.
pub fn mark(messages: &mut [Value], primer: Option<i64>) {
    let system_at = (messages
        .first()
        .and_then(|m| m.get("role").and_then(Value::as_str))
        == Some("system"))
    .then_some(0);
    let primer_at = primer.and_then(|p| {
        messages
            .iter()
            .position(|m| m.get("id").and_then(Value::as_i64) == Some(p))
    });
    let fold_at = messages.iter().rposition(|m| {
        matches!(
            m.get("kind").and_then(Value::as_str),
            Some("fold" | "clear")
        )
    });
    let from = messages
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .map_or(0, |l| l + 1);
    let pending = messages
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, m)| {
            m.get("role").and_then(Value::as_str) == Some("user")
                && crate::history_view::will_fold(
                    m.get("content").and_then(Value::as_str).unwrap_or(""),
                )
        })
        .map(|(i, _)| i);
    let tail = match pending {
        Some(i) => i.checked_sub(1),
        None => messages.len().checked_sub(1),
    };
    let tail = tail.filter(|&i| messages[i].get("role").and_then(Value::as_str) != Some("system"));
    for i in [system_at, primer_at, fold_at, tail].into_iter().flatten() {
        messages[i][CACHE_MARK] = json!(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().expect("object")
    }

    #[test]
    fn the_mode_defaults_to_provider_cache_and_takes_the_two_names() {
        assert_eq!(mode_of(&obj(json!({}))), Ok(Caching::ProviderCache));
        assert_eq!(
            mode_of(&obj(json!({"caching": null}))),
            Ok(Caching::ProviderCache)
        );
        for m in [Caching::None, Caching::ProviderCache] {
            assert_eq!(mode_of(&obj(json!({"caching": m.name()}))), Ok(m));
        }
    }

    #[test]
    fn an_unknown_mode_is_an_error_that_names_it() {
        let err = mode_of(&obj(json!({"caching": "always"}))).unwrap_err();
        assert_eq!(
            err,
            "caching='always' is not one of the legal values: none, provider_cache"
        );
        assert!(mode_of(&obj(json!({"caching": true}))).is_err());
        assert!(mode_of(&obj(json!({"caching": ["none"]}))).is_err());
    }

    #[test]
    fn only_provider_cache_marks() {
        assert!(Caching::ProviderCache.marks());
        assert!(!Caching::None.marks());
    }

    #[test]
    fn the_primer_is_absent_or_a_positive_row_id() {
        assert_eq!(primer_of(&obj(json!({}))), Ok(None));
        assert_eq!(
            primer_of(&obj(json!({"cache_primer_until": null}))),
            Ok(None)
        );
        assert_eq!(
            primer_of(&obj(json!({"cache_primer_until": 7}))),
            Ok(Some(7))
        );
        for bad in [json!(0), json!(-3), json!("7"), json!(2.5), json!(true)] {
            let err = primer_of(&obj(json!({"cache_primer_until": bad}))).unwrap_err();
            assert!(err.starts_with("cache_primer_until="), "{err}");
            assert!(err.ends_with("must be a row id (a positive int)"), "{err}");
        }
    }

    #[test]
    fn mark_flags_the_primer_and_the_last_fold_or_clear() {
        let mut msgs = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": "a", "id": 1}),
            json!({"role": "user", "content": "[fold 5", "id": 5, "kind": "fold"}),
            json!({"role": "assistant", "content": "b", "id": 6}),
            json!({"role": "user", "content": "[fold 9", "id": 9, "kind": "fold"}),
            json!({"role": "user", "content": "c", "id": 10}),
        ];
        mark(&mut msgs, Some(6));
        assert_eq!(
            marked(&msgs),
            vec![0, 3, 4, 5],
            "system, primer, last fold, tail"
        );
    }

    #[test]
    fn mark_with_no_primer_or_no_fold_flags_nothing_extra() {
        let rows = || {
            vec![
                json!({"role": "system", "content": "s"}),
                json!({"role": "user", "content": "a", "id": 1}),
                json!({"role": "user", "content": "b", "id": 2}),
            ]
        };
        let mut none = rows();
        mark(&mut none, None);
        assert_eq!(marked(&none), vec![0, 2]);
        // a primer that is not in the view flags nothing
        let mut gone = rows();
        mark(&mut gone, Some(99));
        assert_eq!(marked(&gone), vec![0, 2]);
    }

    #[test]
    fn a_clear_row_counts_as_a_fold_for_the_marker() {
        let mut msgs = vec![
            json!({"role": "user", "content": "[cleared 4", "id": 4, "kind": "clear"}),
            json!({"role": "user", "content": "x", "id": 5}),
        ];
        mark(&mut msgs, None);
        assert_eq!(marked(&msgs), vec![0, 1]);
    }

    #[test]
    fn folds_runs_in_provider_cache_only() {
        assert!(Caching::ProviderCache.folds());
        assert!(!Caching::None.folds());
    }

    fn big_reply() -> String {
        format!(
            "[reply to 3 · notes read]\n{}",
            "x".repeat(crate::constants::AUTOFOLD_AT * 4 + 4)
        )
    }

    fn marked(msgs: &[Value]) -> Vec<usize> {
        (0..msgs.len())
            .filter(|i| msgs[*i].get(CACHE_MARK) == Some(&json!(true)))
            .collect()
    }

    #[test]
    fn mark_puts_the_tail_mark_on_the_last_message_when_nothing_will_fold() {
        let mut msgs = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": "a", "id": 1}),
            json!({"role": "assistant", "content": "b", "id": 2}),
            json!({"role": "user", "content": "[reply to 3 · notes read]\nsmall"}),
        ];
        mark(&mut msgs, None);
        assert_eq!(marked(&msgs), vec![0, 3]);
    }

    #[test]
    fn mark_puts_the_tail_mark_before_the_first_row_that_will_fold() {
        let mut msgs = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": "a"}),
            json!({"role": "assistant", "content": "b"}),
            json!({"role": "user", "content": big_reply()}),
        ];
        mark(&mut msgs, None);
        assert_eq!(marked(&msgs), vec![0, 2]);
        assert!(msgs[3].get(CACHE_MARK).is_none());
        // a recall reply does the same
        msgs[3]["content"] = json!("[reply to 3 · ai recall]\nx");
        for m in msgs.iter_mut() {
            m.as_object_mut().unwrap().remove(CACHE_MARK);
        }
        mark(&mut msgs, None);
        assert_eq!(marked(&msgs), vec![0, 2]);
        // a will-fold row BEFORE the last assistant row is already answered: ignored
        let mut older = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": big_reply()}),
            json!({"role": "assistant", "content": "b"}),
            json!({"role": "user", "content": "live"}),
        ];
        mark(&mut older, None);
        assert_eq!(marked(&older), vec![0, 3]);
    }

    #[test]
    fn a_system_message_followed_by_a_row_that_will_fold_is_the_only_mark() {
        let mut msgs = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": big_reply()}),
        ];
        mark(&mut msgs, None);
        assert_eq!(marked(&msgs), vec![0]);
    }

    #[test]
    fn the_system_message_is_marked_only_when_it_opens_the_list() {
        let mut msgs = vec![
            json!({"role": "user", "content": "a", "id": 1}),
            json!({"role": "assistant", "content": "b", "id": 2}),
        ];
        mark(&mut msgs, None);
        assert_eq!(marked(&msgs), vec![1]);
    }

    #[test]
    fn render_for_model_carries_cache_mark_and_nothing_else() {
        let out = crate::agent_loop::render_for_model(&[
            json!({"role": "user", "content": "a", "id": 3, "kind": "fold", "cache_mark": true}),
            json!({"role": "user", "content": "b", "id": 4}),
        ]);
        assert_eq!(
            out,
            vec![
                json!({"role": "user", "content": "a", "cache_mark": true}),
                json!({"role": "user", "content": "b"}),
            ]
        );
    }
}
