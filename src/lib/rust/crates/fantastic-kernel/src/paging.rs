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
//! Absent `limit` ⇒ everything, `next: null`. `limit` and `after` are the verb's
//! own declared, typed arguments (a positive integer, a text cursor): what the
//! declaration refuses never reaches this function.

/// The text of a row's `id` — a name as itself, a number as its digits — the identity
/// `page` compares a cursor against. An absent id is the empty text.
#[must_use]
pub(crate) fn id_text(id: Option<&serde_json::Value>) -> String {
    match id {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Apply `limit`/`after` to `items`, keyed by `ident`.
///
/// Returns `(page, next_cursor)`. `next_cursor` is `Some` only when a further
/// page exists, so a caller loops until it is `None`.
///
/// `ident` must already answer the item's identity as text (`id_text`) — `page`
/// compares it against `after` verbatim.
#[must_use]
pub fn page<T, F>(
    items: Vec<T>,
    limit: Option<i64>,
    after: Option<&str>,
    ident: F,
) -> (Vec<T>, Option<String>)
where
    F: Fn(&T) -> String,
{
    let mut items = items;

    // absent or `""` means "from the start"
    if let Some(after) = after.filter(|a| !a.is_empty()) {
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

    // absent: everything — the kernel never guesses a page size on the caller's behalf
    let Some(limit) = limit.and_then(|l| usize::try_from(l).ok()) else {
        return (items, None);
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
