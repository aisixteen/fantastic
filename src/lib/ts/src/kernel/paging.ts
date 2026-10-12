// `page(items, payload, ident)` — the paging primitive (#1). Port of python
// `kernel/_kernel.py::page`, same semantics down to the cursor-miss case.
//
// UNBOUNDED LIST REPLIES ARE A REAL COST, not a conformance box. Measured
// 2026-08-20: reflect bulk was ~42% of an AI agent's context, which is what
// forced `dedupe_stale_reads`, the reflect stub and the point-of-use
// distillation nudge in `ai_core` — every one of those shrinks what is RETAINED
// because nothing could shrink what is SENT. Paging attacks the cause.
//
// The cursor is OPAQUE by construction: it is the last item's IDENTITY, not an
// offset, so a caller cannot compute the next one or skip ahead by arithmetic.
// That also makes it stable under mutation in the one way that matters — a page
// resumes AFTER a named item. It is NOT a snapshot: over a live tree a cursor
// may miss an item deleted mid-walk and may re-show one that moved. This is the
// kernel, not a database; that is stated rather than papered over.
//
// Absent `limit` ⇒ everything, `next: null`. A caller that never learned about
// paging gets exactly what it always got.

import type { Payload } from "./json.ts";

export interface Page<T> {
  page: T[];
  next: string | null;
}

export function page<T>(
  items: readonly T[],
  payload: Payload,
  ident: (item: T) => string = (x) => String(x),
): Page<T> {
  let rest = items.slice();

  // An int cursor (a message id) and its decimal string are the same cursor.
  const raw = payload["after"];
  const after = typeof raw === "number" ? String(raw) : raw;
  if (typeof after === "string" && after !== "") {
    const i = rest.findIndex((it) => ident(it) === after);
    if (i < 0) {
      // A cursor naming something that is GONE: the caller is walking a tree
      // that moved under it. Give it the tail rather than silently the WHOLE
      // list, which would look like a successful restart from the top.
      return { page: [], next: null };
    }
    rest = rest.slice(i + 1);
  }

  const limit = payload["limit"];
  if (typeof limit !== "number" || !Number.isInteger(limit) || limit <= 0) {
    return { page: rest, next: null };
  }
  const head = rest.slice(0, limit);
  const last = head[head.length - 1];
  const next = head.length > 0 && rest.length > limit ? ident(last as T) : null;
  return { page: head, next };
}
