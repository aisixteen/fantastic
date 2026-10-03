import type { Kernel } from "../../kernel/kernel.ts";
import type { Agent } from "../../kernel/agent.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import { storeSafe } from "../../kernel/ledger.ts";
import { pyDumps, pyRepr, pySplitlines } from "../../kernel/pyjson.ts";
import {
  BAD_ARG,
  EXISTS,
  NOT_FOUND,
  UNHANDLED,
  missing,
  refusal,
} from "../../kernel/refusal.ts";

// ram_store — a MEMORY STORE agent held in RAM. Port of python
// `bundled_agents/memory/ram_store` (the canonical one).
//
// A memory store keeps rows for MANY owners (multi-tenant). Every row is keyed
// by `(owner, id)`; the owner is never a field a caller writes — it is WHO
// CALLED: `<kernel guid>|<caller agent id>` for an agent, `<guid>|kernel` for the
// operator and for the kernel's own bookkeeping. So a caller writes ONLY under
// its own key, by construction.
//
// THE TOPOLOGY IS SQLITE'S. The tables below are the ones a durable store
// keeps, with the same keys, the same uniqueness, the same indexes and the same
// foreign-key cascades; only the query language is missing:
//
//   rows       PRIMARY KEY (owner, id)            the row dict, minus `refs`
//              INDEX (owner, id) ordered           paging, `from_last_kind`
//   fold_refs  (owner, fold_id, member_id)         a fold's members, in order
//              FK (owner, fold_id)   → rows ON DELETE CASCADE
//              FK (owner, member_id) → rows ON DELETE CASCADE
//   text_idx   (owner, id) → lowercased text       the FTS counterpart; derived
//
// ACCESS: a caller reads and writes only its OWN key (append / read / search /
// prune). `inject` / `dump` act on the whole store — plain verbs: the kernel's
// call gate decides who may reach this agent and with which verb.
//
// Ids are the KERNEL's: a row carries the int the kernel minted, or none and
// this store takes the next one from the kernel's counter — never an id of its
// own. Every verb mutates the tables synchronously (no await between check and
// write), so a verb is atomic.
//
// Everything here dies with the page. That is what `ram` says.

const OPERATOR = "kernel";

type Row = Payload & { id: number };

/** python `str.strip()` — its whitespace set (`str.isspace`), not JS `trim()`'s:
 *  the \x1c–\x1f separators and \x85 strip, \ufeff does not. */
const PY_WS = "[\\t\\n\\v\\f\\r\\x1c-\\x1f \\x85\\xa0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000]";
const PY_STRIP = new RegExp(`^${PY_WS}+|${PY_WS}+$`, "gu");

/** python `_as_id`: a positive int, or an ASCII-digit string after `strip()` —
 *  `"0"` included (python checks `> 0` only for an int). */
function asId(x: unknown): number | null {
  if (typeof x === "number") return Number.isInteger(x) && x > 0 ? x : null;
  if (typeof x === "string") {
    const t = x.replace(PY_STRIP, "");
    return /^[0-9]+$/.test(t) ? Number(t) : null;
  }
  return null;
}

function isObj(v: unknown): v is Payload {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/** JSON with sorted keys — `match`'s exact-equality comparison. */
function canonical(v: Json): string {
  if (Array.isArray(v)) return `[${v.map(canonical).join(",")}]`;
  if (isObj(v)) {
    return `{${Object.keys(v).sort().map((k) => `${JSON.stringify(k)}:${canonical(v[k] as Json)}`).join(",")}}`;
  }
  return JSON.stringify(v);
}

/** What `search` reads of a row (python `_text_of`): a non-string `content` as
 *  python's `json.dumps(c, ensure_ascii=False)` writes it. */
function textOf(row: Payload): string {
  const c = row["content"];
  let text = typeof c === "string" ? c : pyDumps(c, { ensureAscii: false });
  const title = row["title"];
  if (typeof title === "string" && title !== "") text = `${title}\n${text}`;
  return text;
}

function insort(arr: number[], x: number): void {
  let lo = 0;
  let hi = arr.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if ((arr[mid] as number) < x) lo = mid + 1;
    else hi = mid;
  }
  arr.splice(lo, 0, x);
}

/** The store's tables — RAM maps in sqlite's shape (see the header). */
export class Tables {
  readonly rows = new Map<string, Map<number, Row>>();
  readonly order = new Map<string, number[]>();
  readonly foldRefs = new Map<string, number[]>(); // `${owner}\u0000${fold}` → members
  readonly refByMember = new Map<string, Set<number>>(); // `${owner}\u0000${member}` → folds
  readonly text = new Map<string, string>(); // `${owner}\u0000${id}` → lowercased

  static k(owner: string, id: number): string {
    return `${owner}\u0000${id}`;
  }

  // ── rows ──────────────────────────────────────────────────────────────
  has(owner: string, rid: number): boolean {
    return this.rows.get(owner)?.has(rid) ?? false;
  }

  ids(owner: string): number[] {
    return this.order.get(owner) ?? [];
  }

  raw(owner: string, rid: number): Row | undefined {
    return this.rows.get(owner)?.get(rid);
  }

  get(owner: string, rid: number): Row | null {
    const row = this.raw(owner, rid);
    if (row === undefined) return null;
    const out: Row = { ...row };
    const refs = this.foldRefs.get(Tables.k(owner, rid));
    if (refs !== undefined) out["refs"] = [...refs];
    return out;
  }

  /** All-or-nothing: the caller has validated every row and every ref. */
  insert(owner: string, rows: Row[]): void {
    let table = this.rows.get(owner);
    if (table === undefined) this.rows.set(owner, (table = new Map()));
    let order = this.order.get(owner);
    if (order === undefined) this.order.set(owner, (order = []));
    for (const row of rows) {
      const rid = row.id;
      const refs = row["refs"];
      const stored = storeSafe(
        Object.fromEntries(Object.entries(row).filter(([k]) => k !== "refs")),
      ) as Row;
      table.set(rid, stored);
      insort(order, rid);
      if (Array.isArray(refs)) {
        const members = refs.map((m) => Number(m));
        this.foldRefs.set(Tables.k(owner, rid), members);
        for (const m of members) {
          const key = Tables.k(owner, m);
          let s = this.refByMember.get(key);
          if (s === undefined) this.refByMember.set(key, (s = new Set()));
          s.add(rid);
        }
      }
      this.text.set(Tables.k(owner, rid), textOf(row).toLowerCase());
    }
  }

  /** DELETE FROM rows WHERE owner=? AND id IN doomed — with every cascade.
   *  Returns the rows removed. */
  delete(owner: string, doomedIn: Set<number>): number {
    const table = this.rows.get(owner) ?? new Map<number, Row>();
    const doomed = new Set([...doomedIn].filter((r) => table.has(r)));
    for (const rid of doomed) {
      table.delete(rid);
      this.text.delete(Tables.k(owner, rid));
      // FK fold_refs.fold_id → rows: the fold's member list goes with it.
      const members = this.foldRefs.get(Tables.k(owner, rid)) ?? [];
      this.foldRefs.delete(Tables.k(owner, rid));
      for (const m of members) {
        const s = this.refByMember.get(Tables.k(owner, m));
        if (s !== undefined) {
          s.delete(rid);
          if (s.size === 0) this.refByMember.delete(Tables.k(owner, m));
        }
      }
      // FK fold_refs.member_id → rows: the member leaves every fold naming it.
      const folds = this.refByMember.get(Tables.k(owner, rid)) ?? new Set<number>();
      this.refByMember.delete(Tables.k(owner, rid));
      for (const f of folds) {
        const refs = this.foldRefs.get(Tables.k(owner, f));
        if (refs !== undefined) this.foldRefs.set(Tables.k(owner, f), refs.filter((m) => m !== rid));
      }
    }
    if (doomed.size > 0) {
      this.order.set(owner, (this.order.get(owner) ?? []).filter((i) => !doomed.has(i)));
    }
    return doomed.size;
  }

  // ── dump / inject ─────────────────────────────────────────────────────
  dump(): Payload {
    const owners = [...this.rows.keys()].sort();
    const rows: Json[] = [];
    for (const owner of owners) {
      for (const rid of this.order.get(owner) ?? []) {
        rows.push({ owner, row: { ...(this.rows.get(owner)!.get(rid) as Row) } });
      }
    }
    const refs = [...this.foldRefs.entries()]
      .map(([k, members]) => {
        const [owner, fold] = k.split("\u0000") as [string, string];
        return { owner, fold: Number(fold), members: [...members] };
      })
      .sort((a, b) => (a.owner < b.owner ? -1 : a.owner > b.owner ? 1 : a.fold - b.fold));
    return { rows, fold_refs: refs as unknown as Json };
  }
}

/** Build `Tables` from a `dump` — every key and FK checked; a refusal on the
 *  first violation (nothing half-loaded). */
export function loadTables(tables: unknown): Tables | Payload {
  if (!isObj(tables)) return refusal(BAD_ARG, "inject: `tables` must be a dump dict", { extra: { arg: "tables" } });
  const t = new Tables();
  const rows = tables["rows"] ?? [];
  if (!Array.isArray(rows)) return refusal(BAD_ARG, "inject: tables.rows must be a list", { extra: { arg: "tables" } });
  const byOwner = new Map<string, Row[]>();
  for (const item of rows) {
    if (!isObj(item) || typeof item["owner"] !== "string") {
      return refusal(BAD_ARG, "inject: every row needs {owner, row}", { extra: { arg: "tables" } });
    }
    const row = item["row"];
    if (!isObj(row) || asId(row["id"]) === null) {
      return refusal(BAD_ARG, "inject: every row needs an int id", { extra: { arg: "tables" } });
    }
    const list = byOwner.get(item["owner"]) ?? [];
    list.push(Object.fromEntries(Object.entries(row).filter(([k]) => k !== "refs")) as Row);
    byOwner.set(item["owner"], list);
  }
  for (const [owner, rs] of byOwner) {
    const seen = new Set<number>();
    for (const r of rs) {
      if (seen.has(r.id)) {
        return refusal(EXISTS, `inject: duplicate row (${owner}, ${r.id})`, { extra: { id: r.id } });
      }
      seen.add(r.id);
    }
    t.insert(owner, rs);
  }
  for (const fr of (tables["fold_refs"] as Json[] | undefined) ?? []) {
    if (!isObj(fr)) return refusal(BAD_ARG, "inject: a fold_refs entry must be {owner, fold, members}", { extra: { arg: "tables" } });
    const owner = String(fr["owner"]);
    const fold = asId(fr["fold"]);
    const members = Array.isArray(fr["members"]) ? fr["members"] : [];
    if (fold === null || !t.has(owner, fold)) {
      return refusal(BAD_ARG, `inject: fold_refs names no row ${pyRepr(fold)}`, { extra: { arg: "tables" } });
    }
    if (!members.every((m) => t.has(owner, asId(m) ?? -1))) {
      return refusal(BAD_ARG, `inject: fold ${fold} refs a missing row`, { extra: { arg: "tables" } });
    }
    const ms = members.map((m) => Number(m));
    t.foldRefs.set(Tables.k(owner, fold), ms);
    for (const m of ms) {
      const key = Tables.k(owner, m);
      let s = t.refByMember.get(key);
      if (s === undefined) t.refByMember.set(key, (s = new Set()));
      s.add(fold);
    }
    t.text.set(Tables.k(owner, fold), textOf(t.raw(owner, fold) as Row).toLowerCase());
  }
  return t;
}

/** The store's tables live with the AGENT: one kernel's `mem` is never another
 *  kernel's `mem`, even in one page. RAM by design. */
const TABLES = new WeakMap<Agent, Tables>();

function tablesOf(agent: Agent): Tables {
  let t = TABLES.get(agent);
  if (t === undefined) TABLES.set(agent, (t = new Tables()));
  return t;
}

// ─── who is calling ────────────────────────────────────────────────────

interface Ctx {
  agent: Agent;
  kernel: Kernel;
  guid: string | null;
}

/** The table key of whoever is calling, or a refusal: `<guid>|<caller id>`, the
 *  operator (and the kernel's own bookkeeping) `<guid>|kernel` (python
 *  `_caller`). */
function callerOf(c: Ctx): string | Payload {
  const origin = c.kernel.currentCaller;
  if (c.guid === null) {
    return refusal(NOT_FOUND, "ram_store: this kernel has no guid yet — the store keys every row by it", {
      hint: "boot the kernel root first (it mints the guid)",
    });
  }
  return `${c.guid}|${origin ?? OPERATOR}`;
}

function foldClosure(t: Tables, owner: string, fold: number): Set<number> {
  const out = new Set<number>();
  const stack = [fold];
  while (stack.length > 0) {
    const x = stack.pop() as number;
    if (out.has(x)) continue;
    out.add(x);
    stack.push(...(t.foldRefs.get(Tables.k(owner, x)) ?? []));
  }
  return out;
}

// ─── verbs ─────────────────────────────────────────────────────────────

type Verb = (c: Ctx, payload: Payload) => Json;

const append: Verb = (c, payload) => {
  const owner = callerOf(c);
  if (typeof owner !== "string") return owner;
  const rows = payload["rows"];
  if (!Array.isArray(rows) || rows.length === 0 || !rows.every(isObj)) {
    return refusal(BAD_ARG, "append: `rows` must be a non-empty list of rows", { extra: { arg: "rows" } });
  }
  const t = tablesOf(c.agent);
  const staged: Row[] = [];
  const batch = new Set<number>();
  for (const r of rows as Payload[]) {
    let rid = r["id"];
    if (rid === undefined || rid === null) {
      rid = c.kernel.mintId();
    } else if (typeof rid !== "number" || asId(rid) === null) {
      return refusal(BAD_ARG, `append: a row id is a kernel int, got ${pyRepr(rid)}`, { extra: { arg: "rows" } });
    }
    if (t.has(owner, rid) || batch.has(rid)) {
      return refusal(EXISTS, `append: row ${rid} already exists for this owner`, { extra: { id: rid } });
    }
    batch.add(rid);
    staged.push({ ...r, id: rid });
  }
  for (const r of staged) {
    const refs = r["refs"];
    if (refs === undefined || refs === null) continue;
    if (!Array.isArray(refs) || refs.some((m) => asId(m) === null)) {
      return refusal(BAD_ARG, `append: row ${r.id} refs must be row ids`, { extra: { arg: "rows" } });
    }
    const missingRefs = refs.filter((m) => !(t.has(owner, m as number) || batch.has(m as number)));
    if (missingRefs.length > 0) {
      return refusal(NOT_FOUND, `append: row ${r.id} refs rows that do not exist for this owner: ${pyRepr(missingRefs)}`, {
        extra: { arg: "rows" },
      });
    }
  }
  t.insert(owner, staged);
  return { ids: staged.map((r) => r.id), appended: staged.length };
};

const read: Verb = (c, payload) => {
  const owner = callerOf(c);
  if (typeof owner !== "string") return owner;
  const t = tablesOf(c.agent);
  let ids = [...t.ids(owner)];
  const kindOf = (i: number): Json | undefined => t.raw(owner, i)?.["kind"];
  const flk = payload["from_last_kind"];
  if (typeof flk === "string" && flk !== "") {
    const last = [...ids].reverse().find((i) => kindOf(i) === flk);
    if (last !== undefined) ids = ids.filter((i) => i >= last);
  }
  const kinds = payload["kinds"];
  if (Array.isArray(kinds) && kinds.length > 0) ids = ids.filter((i) => kinds.includes(kindOf(i) as Json));
  const match = payload["match"];
  if (isObj(match) && Object.keys(match).length > 0) {
    ids = ids.filter((i) => {
      const row = t.raw(owner, i) as Row;
      return Object.entries(match).every(([k, v]) => canonical(row[k] ?? null) === canonical(v));
    });
  }
  const want = payload["ids"];
  if (Array.isArray(want)) {
    const wanted = new Set(want.map((x) => asId(x)));
    ids = ids.filter((i) => wanted.has(i));
  }
  const after = asId(payload["after"]);
  const before = asId(payload["before"]);
  if (payload["order"] === "desc") {
    ids.reverse();
    if (after !== null) ids = ids.filter((i) => i < after);
  } else if (after !== null) {
    ids = ids.filter((i) => i > after);
  }
  if (before !== null) ids = ids.filter((i) => i < before);
  const total = ids.length;
  const limit = asId(payload["limit"]);
  const pageIds = limit !== null ? ids.slice(0, limit) : ids;
  const next = limit !== null && total > pageIds.length ? (pageIds[pageIds.length - 1] ?? null) : null;
  return { rows: pageIds.map((i) => t.get(owner, i) as Json), next, total };
};

const search: Verb = (c, payload) => {
  const q = String(payload["query"] ?? "").trim().toLowerCase();
  if (q === "") return missing("search", "query");
  const owner = callerOf(c);
  if (typeof owner !== "string") return owner;
  const t = tablesOf(c.agent);
  let ids = t.ids(owner);
  const want = payload["ids"];
  if (Array.isArray(want) && want.length > 0) {
    const scope = new Set<number>();
    for (const x of want) {
      const rid = asId(x);
      if (rid !== null) for (const m of foldClosure(t, owner, rid)) scope.add(m);
    }
    ids = ids.filter((i) => scope.has(i));
  }
  const limit = asId(payload["limit"]) ?? 100;
  const width = asId(payload["width"]) ?? 200;
  const matches: Json[] = [];
  let total = 0;
  for (const i of ids) {
    if (!(t.text.get(Tables.k(owner, i)) ?? "").includes(q)) continue;
    pySplitlines(textOf(t.raw(owner, i) as Row)).forEach((line, n) => {
      if (line.toLowerCase().includes(q)) {
        total += 1;
        if (matches.length < limit) matches.push({ id: i, line: n + 1, text: line.trim().slice(0, width) });
      }
    });
  }
  return { matches, total, truncated: total > matches.length };
};

const prune: Verb = (c, payload) => {
  const owner = callerOf(c);
  if (typeof owner !== "string") return owner;
  const t = tablesOf(c.agent);
  const before = asId(payload["before_id"]);
  const want = payload["ids"];
  if (before === null && !Array.isArray(want)) return missing("prune", "before_id");
  const keep = Array.isArray(payload["keep"]) ? payload["keep"] : [];
  const wanted = new Set(Array.isArray(want) ? want.map((x) => asId(x)) : []);
  const doomed = new Set<number>();
  for (const i of t.ids(owner)) {
    if (keep.includes(t.raw(owner, i)?.["kind"] as Json)) continue;
    if (before !== null && i < before) doomed.add(i);
    else if (Array.isArray(want) && wanted.has(i)) doomed.add(i);
  }
  return { pruned: t.delete(owner, doomed) };
};

const inject: Verb = (c, payload) => {
  const t = loadTables(payload["tables"]);
  if (!(t instanceof Tables)) return t;
  TABLES.set(c.agent, t);
  let rows = 0;
  for (const m of t.rows.values()) rows += m.size;
  return { rows };
};

const dump: Verb = (c) => {
  return { tables: tablesOf(c.agent).dump() };
};

/** Verb → one-line doc. The kernel answers `reflect` for every agent (no bundle
 *  can), so these reach a reader through the bundle readme below. */
export const RAM_STORE_VERBS: { [verb: string]: string } = {
  boot: "No-op — the tables start empty (RAM) or are filled by `inject`.",
  append:
    "args: rows:list[dict] (req). Appends rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}.",
  read:
    "args: after:int?, before:int?, limit:int?, order:'asc'|'desc'?, kinds:list?, match:dict? (exact field equality), ids:list?, from_last_kind:str? (start at the newest row of that kind). Rows whole, oldest first by default. Returns {rows, next, total}.",
  search:
    "args: query:str (req — case-insensitive substring), ids:list? (rows to search; a fold id includes its members), limit:int? (default 100), width:int? (chars per line, default 200). Every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}. (optional verb)",
  prune:
    "args: before_id:int? and/or ids:list? (req — one of them), keep:list? (kinds kept). Deletes YOUR rows below `before_id` (or exactly `ids`) except kinds in `keep`; every fold ref naming a deleted row goes with it (FK cascade). Returns {pruned}.",
  inject:
    "args: tables:dict (req — a `dump`). Replaces the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows}.",
  dump:
    "No args. The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}]} — exactly what `inject` takes back. Returns {tables}.",
};

const VERBS: { [verb: string]: Verb } = {
  boot: () => null,
  append,
  read,
  search,
  prune,
  inject,
  dump,
};

export const RAM_STORE_README =
  "Memory store in RAM (`ram_store.ts`) — rows per owner (the caller), folds, search; " +
  "ephemeral: everything is lost when the page unloads (durable: false). Multi-tenant: every row " +
  "is keyed `<kernel guid>|<caller id>` (the operator is `kernel`); a caller reads and writes only " +
  "its own key. `inject`/`dump` act on the whole store; the call gate decides who may send them. " +
  "Optional verbs: search. The root's `memory_store_id` and every AI agent's name one.\n\nVerbs:\n" +
  Object.entries(RAM_STORE_VERBS).map(([v, d]) => `- ${v}: ${d}`).join("\n");

export async function handler(id: string, payload: Payload, kernel: Kernel): Promise<Json> {
  const verb = payload["type"];
  const fn = typeof verb === "string" ? VERBS[verb] : undefined;
  if (fn === undefined) {
    return refusal(UNHANDLED, `ram_store: unknown type ${pyRepr(verb)}`, { extra: { verb: verb ?? null } });
  }
  const agent = kernel.get(id);
  if (agent === undefined) throw new Error(`ram_store: no agent '${id}'`);
  return fn({ agent, kernel, guid: kernel.guid }, payload);
}

/** Register the bundle so a `ram_store.ts` record runs in this kernel. */
export function registerRamStore(kernel: Kernel): void {
  kernel.registerBundle("ram_store.ts", handler);
  kernel.setBundleReadme("ram_store.ts", RAM_STORE_README);
}
