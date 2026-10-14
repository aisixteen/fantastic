//! `page` — apply `limit` + an opaque `after` cursor to one list reply.
//!
//! Port of python `kernel/_kernel.py:page` (canonical).
//!
//! UNBOUNDED LIST REPLIES ARE A REAL COST, not a conformance box: reflect bulk
//! can be a large share of an AI agent's context, which is what forced
//! `dedupe_stale_reads`, the reflect stub and the point-of-use
//! distillation nudge in `ai_core` — every one of those shrinks what is
//! RETAINED because nothing could shrink what is SENT. Paging attacks the cause.
//!
//! The cursor is OPAQUE by construction: it is the last item's IDENTITY, not an
//! offset, so a caller cannot compute the next one or skip ahead by arithmetic.
//! That also makes it stable under mutation in the one way that matters — a page
//! resumes AFTER a named item, so items inserted before it are simply not
//! revisited, and items appended after it are picked up. It is NOT a snapshot:
//! over a live tree a cursor may miss an item deleted mid-walk and may re-show
//! one that moved. This is the kernel, not a database; that is stated rather
//! than papered over with a snapshot nobody asked to pay for.
//!
//! Absent `limit` ⇒ everything, `next: null`. So a caller that never learned
//! about paging gets exactly what it always got — the default is unchanged, and
//! the field is inert until asked for.

use serde_json::Value;

/// Apply `limit`/`after` from `payload` to `items`, keyed by `ident`.
///
/// Returns `(page, next_cursor)`. `next_cursor` is `Some` only when a further
/// page exists, so a caller loops until it is `None`.
///
/// `ident` must already answer the item's identity AS PYTHON'S `str()` WOULD
/// (`fantastic_bundle::py_str` on a numeric id) — `page` compares it against
/// `after` verbatim, exactly as python's `str(ident(item)) == after`.
#[must_use]
pub fn page<T, F>(items: Vec<T>, payload: &Value, ident: F) -> (Vec<T>, Option<String>)
where
    F: Fn(&T) -> String,
{
    let mut items = items;

    // PYTHON: `after = payload.get("after")`; a bool is never a cursor
    // (`isinstance(after, bool): after = None`); absent, `None` or `""` means
    // "from the start"; any OTHER value compares as `str(after)` — an int id
    // and its decimal string ARE the same cursor. A non-string `after` is never
    // silently IGNORED (which would page the WHOLE list instead of resuming past
    // it).
    let after = match payload.get("after") {
        None | Some(Value::Null) | Some(Value::Bool(_)) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(v) => Some(fantastic_bundle::py_str(v)),
    };

    if let Some(after) = after {
        match items.iter().position(|it| ident(it) == after) {
            Some(i) => {
                items.drain(..=i);
            }
            None => {
                // A cursor naming something that is gone: the caller is
                // walking a tree that moved under it. Give it the TAIL
                // rather than silently the WHOLE list, which would look like
                // a successful restart from the top.
                return (Vec::new(), None);
            }
        }
    }

    // `limit` must be a positive integer. Anything else (absent, 0, negative,
    // a bool, a string) means "everything" — the kernel never guesses a page
    // size on the caller's behalf.
    let limit = match payload.get("limit") {
        Some(Value::Number(n)) => match n.as_i64() {
            Some(l) if l > 0 => l as usize,
            _ => return (items, None),
        },
        _ => return (items, None),
    };

    if items.len() <= limit {
        return (items, None);
    }
    items.truncate(limit);
    let next = items.last().map(&ident);
    (items, next)
}

#[cfg(test)]
#[path = "paging/tests.rs"]
mod tests;
