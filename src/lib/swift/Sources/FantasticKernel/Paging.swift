// `page` — apply `limit` + an opaque `after` cursor to one list reply.
//
// Port of python `kernel/_kernel.py:page` (canonical).
//
// UNBOUNDED LIST REPLIES ARE A REAL COST, not a conformance box. Measured
// 2026-08-20: reflect bulk was ~42% of an AI agent's context, which is what
// forced the dedupe/stub/distillation work — every one of those shrinks what is
// RETAINED because nothing could shrink what is SENT. Paging attacks the cause.
//
// The cursor is OPAQUE by construction: it is the last item's IDENTITY, not an
// offset, so a caller cannot compute the next one or skip ahead by arithmetic.
// That also makes it stable under mutation in the one way that matters — a page
// resumes AFTER a named item, so items inserted before it are simply not
// revisited, and items appended after it are picked up. It is NOT a snapshot:
// over a live tree a cursor may miss an item deleted mid-walk and may re-show
// one that moved. This is the kernel, not a database; that is stated rather than
// papered over with a snapshot nobody asked to pay for.
//
// Absent `limit` ⇒ everything, `next: null`. A caller that never learned about
// paging gets exactly what it always got.

import FantasticJSON

/// Apply `limit`/`after` from `payload` to `items`, keyed by `ident`.
///
/// Returns the page and the next cursor — `nil` when no further page exists, so
/// a caller loops until it is `nil`.
public func page<T>(
    _ items: [T],
    _ payload: JSON,
    ident: (T) -> String
) -> (page: [T], next: String?) {
    var items = items

    if let after = payload["after"].asString, !after.isEmpty {
        if let i = items.firstIndex(where: { ident($0) == after }) {
            items = Array(items[(i + 1)...])
        } else {
            // A cursor naming something that is GONE: the caller is walking a
            // tree that moved under it. Give it the tail rather than silently
            // the WHOLE list, which would look like a successful restart from
            // the top and hand back the same items forever.
            return ([], nil)
        }
    }

    // `limit` must be a positive integer. Anything else (absent, 0, negative, a
    // bool, a string) means "everything" — the kernel never guesses a page size
    // on the caller's behalf.
    guard case .integer(let raw) = payload["limit"], raw > 0 else {
        return (items, nil)
    }
    let limit = Int(raw)
    if items.count <= limit { return (items, nil) }
    let head = Array(items.prefix(limit))
    return (head, head.last.map(ident))
}
