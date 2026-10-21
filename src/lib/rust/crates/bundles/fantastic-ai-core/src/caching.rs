//! The record's `caching` mode, and the cache flags it asks ai-core to set.
//!
//! `caching` picks how the model view is kept:
//! - `none`: the stored rows, in order, never trimmed. Append-only.
//! - `cleanup` (the default): the view is trimmed. Superseded whole-state reads,
//!   repeated reply bodies and used docs drop out of it. No markers.
//! - `provider_cache`: `none`'s history, plus two markers the provider caches on:
//!   the PRIMER END (the row `cache_primer_until` names) and the LAST FOLD (the last
//!   fold or clear row in the view). Only a backend that declares `provider_cache`
//!   accepts it.
//!
//! ai-core only FLAGS the two messages ([`mark`]). The backend turns the flags into
//! its own markers, and only in `provider_cache`.

use crate::provider::ProviderError;
use fantastic_bundle::py_repr_value;
use serde_json::{json, Map, Value};

/// The legal values of `caching`, in the order the schema lists them.
pub const MODES: [&str; 3] = ["none", "cleanup", "provider_cache"];
/// The mode a record without `caching` runs in.
pub const DEFAULT_MODE: &str = "cleanup";

/// The `caching` field's hint.
pub const CACHING_HINT: &str = "how the conversation is kept in the model's view: none = \
     every stored row, in order, never trimmed (append-only); cleanup = trim superseded \
     reads, repeated reply bodies and used docs from the view; provider_cache = none's \
     history plus the provider's cache markers (the end of the primer row named by \
     cache_primer_until, and the end of the last fold); provider_cache is refused by a \
     backend that cannot cache";

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
    /// The view is trimmed; no markers.
    Cleanup,
    /// Append-only, with the provider's cache markers.
    ProviderCache,
}

impl Caching {
    /// The record's own spelling of the mode.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cleanup => "cleanup",
            Self::ProviderCache => "provider_cache",
        }
    }

    /// Whether the view is trimmed: the three cleanups run.
    #[must_use]
    pub fn cleans(self) -> bool {
        self == Self::Cleanup
    }

    /// Whether the provider is asked for its cache markers.
    #[must_use]
    pub fn marks(self) -> bool {
        self == Self::ProviderCache
    }
}

/// The record's `caching` mode. Absent or null is the default. Any other value is an
/// error — the write refuses it, so only a hand-edited record reaches here, and that
/// fails the turn rather than running a mode nobody chose.
///
/// # Errors
/// The record's `caching` is not one of [`MODES`]; the message names the value.
pub fn mode_of(meta: &Map<String, Value>) -> Result<Caching, String> {
    let raw = match meta.get("caching") {
        None | Some(Value::Null) => return Ok(Caching::Cleanup),
        Some(v) => v,
    };
    match raw.as_str() {
        Some("none") => Ok(Caching::None),
        Some("cleanup") => Ok(Caching::Cleanup),
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

/// The write-time check of the caching fields, on the MERGED record: `provider_cache`
/// only where the backend declares it, and `cache_primer_until` a row id. The mode's
/// own legal values are the schema's enum check, which runs first.
#[must_use]
pub fn refusal(
    meta: &Map<String, Value>,
    record: &Map<String, Value>,
    backend: &str,
    provider_cache: bool,
) -> Option<String> {
    let mut merged = record.clone();
    for (k, v) in meta {
        merged.insert(k.clone(), v.clone());
    }
    if mode_of(&merged) == Ok(Caching::ProviderCache) && !provider_cache {
        return Some(format!(
            "{backend} cannot do provider caching: caching=provider_cache is refused — use \
             none or cleanup"
        ));
    }
    if meta.contains_key("cache_primer_until") {
        if let Err(why) = primer_of(&merged) {
            return Some(why);
        }
    }
    None
}

/// The refusal a turn fails with when its record asks for a caching the backend
/// cannot do. Raised before the turn opens, so nothing is stored.
#[must_use]
pub fn unsupported(name: &str) -> ProviderError {
    ProviderError::value_error(format!(
        "{name}: caching=provider_cache is not supported by this backend — use none or cleanup"
    ))
}

/// Flag the messages the provider's markers land on, in the list it is handed:
/// `cache_primer` on the message of the row `primer` names (no flag when that row is
/// not in the view), and `cache_fold` on the LAST message of a fold or clear row.
/// Both flags are on the message's own text, so a backend can cut its block there.
pub fn mark(messages: &mut [Value], primer: Option<i64>) {
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
    if let Some(i) = primer_at {
        messages[i]["cache_primer"] = json!(true);
    }
    if let Some(i) = fold_at {
        messages[i]["cache_fold"] = json!(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().expect("object")
    }

    #[test]
    fn the_mode_defaults_to_cleanup_and_takes_the_three_names() {
        assert_eq!(mode_of(&obj(json!({}))), Ok(Caching::Cleanup));
        assert_eq!(
            mode_of(&obj(json!({"caching": null}))),
            Ok(Caching::Cleanup)
        );
        assert_eq!(mode_of(&obj(json!({"caching": "none"}))), Ok(Caching::None));
        assert_eq!(
            mode_of(&obj(json!({"caching": "cleanup"}))),
            Ok(Caching::Cleanup)
        );
        assert_eq!(
            mode_of(&obj(json!({"caching": "provider_cache"}))),
            Ok(Caching::ProviderCache)
        );
        for m in [Caching::None, Caching::Cleanup, Caching::ProviderCache] {
            assert_eq!(mode_of(&obj(json!({"caching": m.name()}))), Ok(m));
        }
    }

    #[test]
    fn an_unknown_mode_is_an_error_that_names_it() {
        let err = mode_of(&obj(json!({"caching": "always"}))).unwrap_err();
        assert_eq!(
            err,
            "caching='always' is not one of the legal values: none, cleanup, provider_cache"
        );
        assert!(mode_of(&obj(json!({"caching": true}))).is_err());
        assert!(mode_of(&obj(json!({"caching": ["cleanup"]}))).is_err());
    }

    #[test]
    fn only_cleanup_trims_and_only_provider_cache_marks() {
        assert!(Caching::Cleanup.cleans());
        assert!(!Caching::None.cleans());
        assert!(!Caching::ProviderCache.cleans());
        assert!(Caching::ProviderCache.marks());
        assert!(!Caching::None.marks());
        assert!(!Caching::Cleanup.marks());
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
    fn provider_cache_is_refused_where_the_backend_cannot_cache() {
        let rec = obj(json!({"memory_store_id": "s"}));
        let asks = obj(json!({"caching": "provider_cache"}));
        let why = refusal(&asks, &rec, "ollama_backend", false).expect("refused");
        assert!(why.contains("ollama_backend"), "{why}");
        assert!(why.contains("provider_cache"), "{why}");
        assert_eq!(refusal(&asks, &rec, "anthropic_backend", true), None);
        // none and cleanup go anywhere
        for mode in ["none", "cleanup"] {
            let m = obj(json!({"caching": mode}));
            assert_eq!(refusal(&m, &rec, "ollama_backend", false), None);
        }
        // a record already in provider_cache, named again on a backend that cannot
        assert!(refusal(&obj(json!({"recent_n": 4})), &asks, "ollama_backend", false).is_some());
    }

    #[test]
    fn a_primer_named_on_the_write_must_be_a_row_id() {
        let rec = obj(json!({}));
        assert!(refusal(&obj(json!({"cache_primer_until": 3})), &rec, "b", true).is_none());
        assert!(refusal(&obj(json!({"cache_primer_until": "x"})), &rec, "b", true).is_some());
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
        assert_eq!(msgs[3]["cache_primer"], json!(true));
        assert_eq!(msgs[4]["cache_fold"], json!(true), "the LAST fold");
        assert!(msgs[2].get("cache_fold").is_none());
        assert!(msgs[1].get("cache_primer").is_none());
        assert!(msgs[0].get("cache_primer").is_none());
    }

    #[test]
    fn mark_with_no_primer_or_no_fold_flags_nothing_extra() {
        let mut none = vec![json!({"role": "user", "content": "a", "id": 1})];
        mark(&mut none, None);
        assert!(none[0].get("cache_primer").is_none());
        assert!(none[0].get("cache_fold").is_none());
        // a primer that is not in the view flags nothing
        let mut gone = vec![json!({"role": "user", "content": "a", "id": 1})];
        mark(&mut gone, Some(99));
        assert!(gone[0].get("cache_primer").is_none());
    }

    #[test]
    fn a_clear_row_counts_as_a_fold_for_the_marker() {
        let mut msgs = vec![
            json!({"role": "user", "content": "[cleared 4", "id": 4, "kind": "clear"}),
            json!({"role": "user", "content": "x", "id": 5}),
        ];
        mark(&mut msgs, None);
        assert_eq!(msgs[0]["cache_fold"], json!(true));
    }
}
