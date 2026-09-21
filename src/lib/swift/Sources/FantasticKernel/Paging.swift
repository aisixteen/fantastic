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

    // C17 (R2-RFL-2): the cursor is python `str(after)` for ANY value that is
    // not `None`, not a bool (either one — python's `isinstance(after, bool)`
    // check fires before the stringify, so `true`/`false` are NO cursor, the
    // same as absent) and not `""` — an int's digits, a float's python repr
    // (`2.0`, never truncated to `2`), a list/dict's python container repr.
    // This used to read `.asString ?? .asInt.map(String.init)`, which left a
    // float/array/object cursor silently absent (→ the WHOLE list) instead of
    // python's `[]` (a cursor that stringifies to something no item's id
    // matches — measured: `after=1.5` py `[]` / this used to be `full`;
    // `after=["c1"]` py `[]` / this used to be `full`; `after=2.0` py `[]`
    // via the literal string `"2.0"`, not by luckily truncating to an int
    // that happened to match nothing).
    let cursor: String?
    switch payload["after"] {
    case .null, .bool: cursor = nil
    case .string(let s): cursor = s.isEmpty ? nil : s
    case let other: cursor = pyStr(other)
    }
    if let after = cursor {
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
