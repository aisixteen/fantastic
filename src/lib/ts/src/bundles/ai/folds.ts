import type { Json } from "../../kernel/json.ts";
import type { Turn } from "./context.ts";
import { extractToolCalls } from "./tool_parse.ts";

// FOLDS — a compaction MARK, not a deletion. Port of python `ai_core/folds.py`.
//
// A fold is an ordinary appended row that names the rows it collapses (`refs`),
// carries a title (what the model reads every turn), a body (a short summary),
// and stays one descent from its members: `grep` scoped to a fold id and
// `history {fold}` expand to its refs. Nothing is rewritten, nothing is lost, and
// the history becomes a tree of marks the model walks down as far as it needs.
//
// CHAINS ARE DERIVED, NEVER STORED. A chain is a root row plus everything
// reachable from it through `parent`; this module walks the links when a
// projection needs to know what is closed.

export const FOLD_TAG = "[fold ";

type Id = Json | undefined;

/** Row indices grouped by root, in root arrival order. A row whose parent is
 *  unknown (absent, or pointing outside `rows`) is its own root. */
export function chains(rows: Turn[]): number[][] {
  const byId = new Map<Id, number>();
  rows.forEach((r, i) => {
    if (r["id"] !== undefined && r["id"] !== null) byId.set(r["id"], i);
  });
  const rootOf = new Map<number, number>();
  const root = (i: number): number => {
    const known = rootOf.get(i);
    if (known !== undefined) return known;
    const seen = [i];
    let cur = i;
    for (;;) {
      const par = rows[cur]!["parent"];
      const j = par !== undefined && par !== null ? byId.get(par) : undefined;
      if (j === undefined || seen.includes(j)) break;
      seen.push(j);
      cur = j;
    }
    for (const k of seen) rootOf.set(k, cur);
    return cur;
  };
  const groups = new Map<number, number[]>();
  for (let i = 0; i < rows.length; i++) {
    const r = root(i);
    const g = groups.get(r) ?? [];
    g.push(i);
    groups.set(r, g);
  }
  return [...groups.keys()].sort((a, b) => a - b).map((k) => groups.get(k)!);
}

/** A chain is OPEN while its last row is an assistant turn that made a `send` —
 *  a reply is still owed. A failed call settles as a reply row, so it closes a
 *  chain the same way an answer does. */
export function isOpen(rows: Turn[], chain: number[]): boolean {
  const last = rows[chain[chain.length - 1] as number]!;
  if (last["role"] !== "assistant") return false;
  return extractToolCalls(String(last["content"] ?? "")).some((c) => c.name === "send");
}

export function isFold(row: Turn): boolean {
  return row["kind"] === "fold";
}

/** Every id named by any fold's refs — transitively, since a fold may ref folds. */
export function covered(rows: Turn[]): Set<Id> {
  const out = new Set<Id>();
  for (const r of rows) {
    if (isFold(r)) for (const x of (r["refs"] as Json[] | undefined) ?? []) out.add(x);
  }
  return out;
}

/** A fold title lives inside the tag; it may not close it or quote itself. */
export function cleanTitle(t: unknown): string {
  return String(t).replace(/]/g, ")").replace(/"/g, "'").split(/\s+/).filter(Boolean).join(" ").slice(0, 140) ||
    "(untitled)";
}

/** A fold row. Its id is MINTED BY THE KERNEL and passed in — a fold is a message
 *  like any other, never self-named. It sits where its members sat: its parent
 *  is its first member's parent. */
export function makeFold(
  _selfId: string,
  refs: Json[],
  title: string,
  body: string,
  degraded: boolean,
  opts: { id: number; lvl?: number; parent?: Json },
): Turn {
  const lvl = opts.lvl ?? 1;
  const t = cleanTitle(title);
  const headLine = `${FOLD_TAG}${opts.id} · ${refs.length} refs · L${lvl}] "${t}"`;
  const note = degraded ? "  (summary unavailable — refs and titles only)" : "";
  return {
    role: "user",
    id: opts.id,
    parent: opts.parent ?? null,
    kind: "fold",
    refs: [...refs],
    title: t,
    body,
    level: lvl, // 1 = rows, 2 = a fold among its members, …
    content: `${headLine}${note}\n${body}`,
  };
}

/** The view with folded rows hidden and each fold sitting where its first ref
 *  was. Deterministic in `rows`, so a view renders the same bytes until a new
 *  fold lands. */
export function render(rows: Turn[]): Turn[] {
  const cov = covered(rows);
  const firstPos = new Map<Id, number>();
  rows.forEach((r, i) => {
    const rid = r["id"];
    if (cov.has(rid) && !firstPos.has(rid)) firstPos.set(rid, i);
  });
  const place = new Map<number, Turn[]>();
  for (const r of rows) {
    if (!isFold(r)) continue;
    const at = ((r["refs"] as Json[] | undefined) ?? [])
      .map((x) => firstPos.get(x))
      .filter((x): x is number => x !== undefined);
    if (at.length > 0 && !cov.has(r["id"])) {
      const pos = Math.min(...at);
      const list = place.get(pos) ?? [];
      list.push(r);
      place.set(pos, list);
    }
  }
  const out: Turn[] = [];
  rows.forEach((r, i) => {
    for (const f of place.get(i) ?? []) out.push(f);
    if (cov.has(r["id"]) || isFold(r)) return; // hidden, or a fold rendered at its first ref
    out.push(r);
  });
  return out;
}

/** Replace fold ids with their refs, recursively — the descent path. */
export function expandIds(rows: Turn[], ids: Json[]): Json[] {
  const folds = new Map<Id, Turn>();
  for (const r of rows) if (isFold(r)) folds.set(r["id"], r);
  const out: Json[] = [];
  const stack = [...ids];
  const seen = new Set<Id>();
  while (stack.length > 0) {
    const x = stack.shift() as Json;
    if (seen.has(x)) continue;
    seen.add(x);
    const f = folds.get(x);
    if (f !== undefined) stack.unshift(...((f["refs"] as Json[] | undefined) ?? []));
    else out.push(x);
  }
  return out;
}

/** 1 for a fold of rows, 2 for a fold of folds, and so on. */
export function level(rows: Turn[], foldRow: Turn): number {
  const byId = new Map<Id, Turn>();
  for (const r of rows) if (r["id"] !== undefined && r["id"] !== null) byId.set(r["id"], r);
  let best = 0;
  for (const ref of (foldRow["refs"] as Json[] | undefined) ?? []) {
    const r = byId.get(ref);
    if (r !== undefined && isFold(r)) best = Math.max(best, level(rows, r));
  }
  return best + 1;
}

/** Chars handed to the summarizer per fold — never a 60k reply. */
export const SUMMARY_INPUT_CAP = 24_000;

/** What the summarizer reads. CAPPED per row and in total: a chain with big
 *  inline rows would otherwise make the fold call itself the reply that does not
 *  fit. */
export function chainText(rows: Turn[], chain: number[]): string {
  const per = Math.max(400, Math.floor(SUMMARY_INPUT_CAP / Math.max(1, chain.length)));
  const items = chain.map((i) => {
    let c = String(rows[i]!["content"] ?? "");
    if (c.length > per) {
      c = c.slice(0, Math.floor(per / 2)) + ` …[${c.length - per} chars elided]… ` + c.slice(-Math.floor(per / 2));
    }
    return { role: rows[i]!["role"] ?? null, id: rows[i]!["id"] ?? null, content: c };
  });
  return JSON.stringify(items).slice(0, SUMMARY_INPUT_CAP);
}
