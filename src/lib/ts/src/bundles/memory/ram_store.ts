import type { Kernel } from "../../kernel/kernel.ts";
import type { Agent } from "../../kernel/agent.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import { storeSafe } from "../../kernel/ledger.ts";
import {
  BAD_ARG,
  EXISTS,
  NOT_FOUND,
  UNAUTHORIZED,
  UNHANDLED,
  missing,
  refusal,
} from "../../kernel/refusal.ts";

// ram_store — a MEMORY STORE agent held in RAM. Port of python
// `bundled_agents/memory/ram_store` (the canonical one).
//
// A memory store keeps rows for MANY owners (multi-tenant). Every row is keyed
// by `(owner, id)`; the owner is never a field a caller writes — it is WHO
// CALLED: `<kernel guid>|<caller agent id>` for a local caller and
// `<guid>|kernel` for the operator. So a caller writes ONLY under its own key, by
// construction. (This kernel has no inbound bridge leg, so there is no remote
// caller to key by its qualified address.)
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
//   grants     PRIMARY KEY gid                     {owner, reader, scope, mode,
//                                                   anchor, as_of, until, why, …}
//              FK (owner, anchor) → rows ON DELETE CASCADE
//              UNIQUE (owner, reader, scope, mode) an identical share is one grant
//   text_idx   (owner, id) → lowercased text       the FTS counterpart; derived
//
// ACCESS, nothing implicit:
//   - any caller: its OWN key (append / read / search / prune / share / unshare /
//     grants), plus rows another owner GRANTED it (`read`/`search` with `grant`).
//   - `operator: true` on the record serves the operator at all (as `kernel`).
//   - `admins: [ids]` (`"kernel"` = the operator) may name another `owner` and
//     may `inject` / `dump` the whole store.
// The kernel's call gate still decides who may reach this agent at all.
//
// Ids are the KERNEL's: a row carries the int the kernel minted, or none and
// this store takes the next one from the kernel's counter — never an id of its
// own. Every verb mutates the tables synchronously (no await between check and
// write), so a verb is atomic.
//
// Everything here dies with the page. That is what `ram` says.

const OPERATOR = "kernel";
const MODES = ["frozen", "live"] as const;
const SCOPE_KINDS = ["head_until", "range", "fold", "row"] as const;
// kinds whose `refs` name members (the fold_refs table): a fold, a clear-fold.
const REF_KINDS = ["fold", "clear"];

type Row = Payload & { id: number };
type Scope = Payload;
interface Grant {
  grant: number;
  owner: string;
  reader: string;
  scope: Scope;
  mode: string;
  anchor: number;
  as_of: number | null;
  until: null;
  why: string | null;
  created_by: number | null;
  ts: number;
}

function asId(x: unknown): number | null {
  if (typeof x === "number") return Number.isInteger(x) && x > 0 ? x : null;
  if (typeof x === "string" && /^\d+$/.test(x.trim())) {
    const n = Number(x.trim());
    return n > 0 ? n : null;
  }
  return null;
}

function isObj(v: unknown): v is Payload {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/** JSON with sorted keys — the UNIQUE (owner, reader, scope, mode) key. */
function canonical(v: Json): string {
  if (Array.isArray(v)) return `[${v.map(canonical).join(",")}]`;
  if (isObj(v)) {
    return `{${Object.keys(v).sort().map((k) => `${JSON.stringify(k)}:${canonical(v[k] as Json)}`).join(",")}}`;
  }
  return JSON.stringify(v);
}

function textOf(row: Payload): string {
  const c = row["content"];
  let text = typeof c === "string" ? c : JSON.stringify(c ?? null);
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
  readonly grants = new Map<number, Grant>();
  grantSeq = 0;
  readonly grantByAnchor = new Map<string, Set<number>>(); // `${owner}\u0000${anchor}`
  readonly grantKey = new Map<string, number>();
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
   *  Returns [rows removed, grants removed]. */
  delete(owner: string, doomedIn: Set<number>): [number, number] {
    const table = this.rows.get(owner) ?? new Map<number, Row>();
    const doomed = new Set([...doomedIn].filter((r) => table.has(r)));
    let grantsGone = 0;
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
      // FK grants.anchor → rows: a grant anchored here is gone.
      const anchored = this.grantByAnchor.get(Tables.k(owner, rid)) ?? new Set<number>();
      this.grantByAnchor.delete(Tables.k(owner, rid));
      for (const gid of [...anchored]) if (this.dropGrant(gid)) grantsGone += 1;
    }
    if (doomed.size > 0) {
      this.order.set(owner, (this.order.get(owner) ?? []).filter((i) => !doomed.has(i)));
    }
    return [doomed.size, grantsGone];
  }

  // ── grants ────────────────────────────────────────────────────────────
  private static grantKeyOf(g: { owner: string; reader: string; scope: Scope; mode: string }): string {
    return [g.owner, g.reader, canonical(g.scope), g.mode].join("\u0000");
  }

  addGrant(g: Omit<Grant, "grant">): [number, boolean] {
    const key = Tables.grantKeyOf(g);
    const existing = this.grantKey.get(key);
    if (existing !== undefined) return [existing, false];
    this.grantSeq += 1;
    const gid = this.grantSeq;
    this.indexGrant({ ...g, grant: gid });
    return [gid, true];
  }

  indexGrant(g: Grant): void {
    this.grants.set(g.grant, g);
    this.grantKey.set(Tables.grantKeyOf(g), g.grant);
    const ak = Tables.k(g.owner, g.anchor);
    let s = this.grantByAnchor.get(ak);
    if (s === undefined) this.grantByAnchor.set(ak, (s = new Set()));
    s.add(g.grant);
  }

  dropGrant(gid: number): boolean {
    const g = this.grants.get(gid);
    if (g === undefined) return false;
    this.grants.delete(gid);
    this.grantKey.delete(Tables.grantKeyOf(g));
    const ak = Tables.k(g.owner, g.anchor);
    const s = this.grantByAnchor.get(ak);
    if (s !== undefined) {
      s.delete(gid);
      if (s.size === 0) this.grantByAnchor.delete(ak);
    }
    return true;
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
    const grants = [...this.grants.entries()]
      .sort(([a], [b]) => a - b)
      .map(([, g]) => ({ ...g }) as unknown as Json);
    return { rows, fold_refs: refs as unknown as Json, grants };
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
      return refusal(BAD_ARG, `inject: fold_refs names no row ${JSON.stringify(fr["fold"])}`, { extra: { arg: "tables" } });
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
  for (const g of (tables["grants"] as Json[] | undefined) ?? []) {
    if (!isObj(g) || typeof g["owner"] !== "string" || !t.has(g["owner"], asId(g["anchor"]) ?? -1)) {
      return refusal(BAD_ARG, "inject: a grant's anchor names no row", { extra: { arg: "tables" } });
    }
    const gid = asId(g["grant"]);
    if (gid === null) return refusal(BAD_ARG, "inject: a grant needs an int `grant`", { extra: { arg: "tables" } });
    t.indexGrant({ ...(g as unknown as Grant), grant: gid });
    t.grantSeq = Math.max(t.grantSeq, gid);
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

/** An owner/reader NAME as a table key: a qualified `guid|id` verbatim, a bare
 *  local id under this kernel's guid. */
function keyOf(c: Ctx, name: unknown): string | null {
  if (typeof name !== "string" || name.trim() === "") return null;
  const n = name.trim();
  if (n.includes("|")) return n;
  return c.guid !== null ? `${c.guid}|${n}` : null;
}

/** A key as a caller reads it: the bare id for this kernel, qualified otherwise. */
function display(c: Ctx, key: string): string {
  return c.guid !== null && key.startsWith(`${c.guid}|`) ? key.slice(c.guid.length + 1) : key;
}

/** [key, display] for whoever is calling, or a refusal. */
function callerOf(c: Ctx): [string, string] | Payload {
  if (c.guid === null) {
    return refusal(NOT_FOUND, "ram_store: this kernel has no guid yet — the store keys every row by it", {
      hint: "boot the kernel root first (kernel_start boots it; the root's boot mints the guid)",
    });
  }
  const origin = c.kernel.currentCaller;
  if (origin === null) {
    if (c.agent.meta["operator"] !== true) {
      return refusal(UNAUTHORIZED, `${c.agent.id}: the operator is not served here`, {
        hint: `set operator=true on ${c.agent.id} to serve the operator (as 'kernel')`,
        extra: { side: "store" },
      });
    }
    return [`${c.guid}|${OPERATOR}`, OPERATOR];
  }
  return [`${c.guid}|${origin}`, origin];
}

function isAdmin(c: Ctx, who: string): boolean {
  const a = c.agent.meta["admins"];
  const admins = typeof a === "string" ? [a] : Array.isArray(a) ? a : [];
  return admins.includes(who);
}

/** [owner key, caller key, caller display] for a verb that acts on ONE owner:
 *  the caller's own key, or `owner` when the caller is an admin. */
function ownerFor(c: Ctx, payload: Payload, verb: string): [string, string, string] | Payload {
  const who = callerOf(c);
  if (!Array.isArray(who)) return who;
  const [me, disp] = who;
  const name = payload["owner"];
  if (name === undefined || name === null) return [me, me, disp];
  const owner = keyOf(c, name);
  if (owner === null) return refusal(BAD_ARG, `${verb}: \`owner\` must be an agent id`, { extra: { arg: "owner" } });
  if (owner !== me && !isAdmin(c, disp)) {
    return refusal(UNAUTHORIZED, `${verb}: '${disp}' may act only on its own rows, not '${String(name)}'`, {
      hint:
        "a caller writes and reads its OWN key; another owner's rows are reached through a " +
        `grant it gives (\`share\`), or by an agent listed in ${c.agent.id}.admins`,
      extra: { side: "store" },
    });
  }
  return [owner, me, disp];
}

// ─── scopes ────────────────────────────────────────────────────────────

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

/** The rows a grant reaches, as of its bound (frozen) or following the head (live). */
function scopeIds(t: Tables, g: Grant): Set<number> {
  const live = g.mode === "live";
  const ids = t.ids(g.owner);
  const all = new Set(ids);
  const scope = g.scope;
  let base: Set<number>;
  if ("row" in scope) {
    base = new Set([scope["row"] as number].filter((x) => all.has(x)));
  } else if ("fold" in scope) {
    base = new Set([...foldClosure(t, g.owner, scope["fold"] as number)].filter((x) => all.has(x)));
  } else if ("range" in scope) {
    const r = scope["range"] as Payload;
    const lo = r["from"] as number;
    const hi = r["to"] as number;
    base = new Set(ids.filter((i) => i >= lo && (live || i <= hi)));
  } else {
    const n = scope["head_until"] as number;
    base = new Set(ids.filter((i) => live || i <= n));
  }
  if (!live && g.as_of !== null) base = new Set([...base].filter((i) => i <= (g.as_of as number)));
  return base;
}

/** [normalised scope, anchor] — the anchor is the scope's LOWEST existing row, so
 *  a prune that touches any part of the scope removes the grant (FK cascade). */
function parseScope(t: Tables, owner: string, scope: unknown): [Scope, number] | Payload {
  const keys = isObj(scope) ? Object.keys(scope) : [];
  if (!isObj(scope) || keys.length !== 1 || !(SCOPE_KINDS as readonly string[]).includes(keys[0] as string)) {
    return refusal(
      BAD_ARG,
      "share: `scope` must be ONE of {head_until: <row>}, {range: {from, to}}, {fold: <id>}, {row: <id>} — a descriptor, never a list of messages",
      { extra: { arg: "scope" } },
    );
  }
  const kind = keys[0] as string;
  const val = scope[kind];
  const ids = t.ids(owner);
  if (kind === "range") {
    if (!isObj(val)) return refusal(BAD_ARG, "share: range must be {from, to}", { extra: { arg: "scope" } });
    const lo = asId(val["from"]);
    const hi = asId(val["to"]);
    if (lo === null || hi === null || lo > hi) {
      return refusal(BAD_ARG, "share: range needs from <= to (row ids)", { extra: { arg: "scope" } });
    }
    const inside = ids.filter((i) => lo <= i && i <= hi);
    if (inside.length === 0) {
      return refusal(NOT_FOUND, `share: none of YOUR rows lie in ${lo}..${hi}`, { extra: { arg: "scope" } });
    }
    return [{ range: { from: lo, to: hi } }, inside[0] as number];
  }
  const rid = asId(val);
  if (rid === null) return refusal(BAD_ARG, `share: ${kind} must be a row id`, { extra: { arg: "scope" } });
  if (kind === "head_until") {
    if (ids.length === 0 || (ids[0] as number) > rid) {
      return refusal(NOT_FOUND, `share: you have no rows up to ${rid}`, { extra: { arg: "scope" } });
    }
    return [{ head_until: rid }, ids[0] as number];
  }
  if (!t.has(owner, rid)) {
    // ZERO TOLERANCE: a row that is not under YOUR key is not yours to share —
    // a foreign row, or one shared WITH you, simply is not found here.
    return refusal(NOT_FOUND, `share: row ${rid} is not one of YOUR rows — only rows you own can be shared`, {
      extra: { arg: "scope" },
    });
  }
  if (kind === "fold" && !REF_KINDS.includes(String(t.raw(owner, rid)?.["kind"]))) {
    return refusal(BAD_ARG, `share: row ${rid} is not a fold`, { extra: { arg: "scope" } });
  }
  return [{ [kind]: rid }, rid];
}

function grantView(c: Ctx, g: Grant): Payload {
  return {
    grant: g.grant,
    owner: display(c, g.owner),
    reader: display(c, g.reader),
    scope: g.scope,
    mode: g.mode,
    as_of: g.as_of,
    until: g.until,
    why: g.why,
    created_by: g.created_by,
    ts: g.ts,
  };
}

/** [owner key, allowed ids | null = all] for a read-side verb: `grant` reads
 *  another owner's scope; else the caller's own key (or `owner`, admin). */
function readable(c: Ctx, t: Tables, payload: Payload, verb: string): [string, Set<number> | null] | Payload {
  const gid = payload["grant"];
  if (gid === undefined || gid === null) {
    const o = ownerFor(c, payload, verb);
    return Array.isArray(o) ? [o[0], null] : o;
  }
  const who = callerOf(c);
  if (!Array.isArray(who)) return who;
  const [me, disp] = who;
  const g = t.grants.get(asId(gid) ?? -1);
  if (g === undefined) {
    return refusal(NOT_FOUND, `${verb}: grant ${JSON.stringify(gid)} does not exist — revoked (unshare) or its rows were pruned`, {
      extra: { arg: "grant" },
    });
  }
  if (g.reader !== me && !isAdmin(c, disp)) {
    return refusal(UNAUTHORIZED, `${verb}: grant ${g.grant} was given to '${display(c, g.reader)}', not '${disp}'`, {
      extra: { side: "store" },
    });
  }
  return [g.owner, scopeIds(t, g)];
}

// ─── verbs ─────────────────────────────────────────────────────────────

type Verb = (c: Ctx, payload: Payload) => Json;

const append: Verb = (c, payload) => {
  const o = ownerFor(c, payload, "append");
  if (!Array.isArray(o)) return o;
  const owner = o[0];
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
      return refusal(BAD_ARG, `append: a row id is a kernel int, got ${JSON.stringify(rid)}`, { extra: { arg: "rows" } });
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
      return refusal(NOT_FOUND, `append: row ${r.id} refs rows that do not exist for this owner: ${JSON.stringify(missingRefs)}`, {
        extra: { arg: "rows" },
      });
    }
  }
  t.insert(owner, staged);
  return { ids: staged.map((r) => r.id), appended: staged.length };
};

const read: Verb = (c, payload) => {
  const t = tablesOf(c.agent);
  const r = readable(c, t, payload, "read");
  if (!Array.isArray(r)) return r;
  const [owner, allowed] = r;
  let ids = [...t.ids(owner)];
  if (allowed !== null) ids = ids.filter((i) => allowed.has(i));
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
  const t = tablesOf(c.agent);
  const r = readable(c, t, payload, "search");
  if (!Array.isArray(r)) return r;
  const [owner, allowed] = r;
  let ids = t.ids(owner);
  if (allowed !== null) ids = ids.filter((i) => allowed.has(i));
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
    textOf(t.raw(owner, i) as Row).split(/\r?\n/).forEach((line, n) => {
      if (line.toLowerCase().includes(q)) {
        total += 1;
        if (matches.length < limit) matches.push({ id: i, line: n + 1, text: line.trim().slice(0, width) });
      }
    });
  }
  return { matches, total, truncated: total > matches.length };
};

const share: Verb = (c, payload) => {
  const who = callerOf(c);
  if (!Array.isArray(who)) return who;
  const [me] = who;
  const reader = keyOf(c, payload["with"]);
  if (reader === null) return missing("share", "with");
  if (reader === me) return refusal(BAD_ARG, "share: you already read your own rows", { extra: { arg: "with" } });
  const mode = payload["mode"] ?? "frozen";
  if (!(MODES as readonly Json[]).includes(mode)) {
    return refusal(BAD_ARG, `share: mode must be one of ${MODES.join(", ")}`, { extra: { arg: "mode" } });
  }
  const t = tablesOf(c.agent);
  const parsed = parseScope(t, me, payload["scope"]);
  if (!Array.isArray(parsed)) return parsed;
  const [scope, anchor] = parsed;
  const ids = t.ids(me);
  const why = String(payload["why"] ?? "").split(/\s+/).filter(Boolean).join(" ").slice(0, 300);
  const [gid, created] = t.addGrant({
    owner: me,
    reader,
    scope,
    mode: mode as string,
    anchor,
    as_of: mode === "frozen" && ids.length > 0 ? (ids[ids.length - 1] as number) : null,
    until: null, // reserved: a row limit / time bound — unused
    why: why === "" ? null : why,
    created_by: c.kernel.currentTask,
    ts: Date.now() / 1000,
  });
  const g = t.grants.get(gid) as Grant;
  return { grant: gid, anchor, as_of: g.as_of, created };
};

const unshare: Verb = (c, payload) => {
  const who = callerOf(c);
  if (!Array.isArray(who)) return who;
  const [me, disp] = who;
  const gid = asId(payload["grant"]);
  if (gid === null) return missing("unshare", "grant");
  const t = tablesOf(c.agent);
  const g = t.grants.get(gid);
  if (g === undefined) return refusal(NOT_FOUND, `unshare: no grant ${gid}`, { extra: { arg: "grant" } });
  if (g.owner !== me && !isAdmin(c, disp)) {
    return refusal(UNAUTHORIZED, `unshare: grant ${gid} is not yours to revoke`, { extra: { side: "store" } });
  }
  t.dropGrant(gid);
  return { unshared: true, grant: gid };
};

const grants: Verb = (c, payload) => {
  const who = callerOf(c);
  if (!Array.isArray(who)) return who;
  const [me] = who;
  const side = payload["side"];
  if (side !== "given" && side !== "received") {
    return refusal(BAD_ARG, "grants: side must be 'given' or 'received'", { extra: { arg: "side" } });
  }
  const t = tablesOf(c.agent);
  return {
    grants: [...t.grants.entries()]
      .sort(([a], [b]) => a - b)
      .filter(([, g]) => (side === "given" ? g.owner : g.reader) === me)
      .map(([, g]) => grantView(c, g)),
  };
};

const prune: Verb = (c, payload) => {
  const o = ownerFor(c, payload, "prune");
  if (!Array.isArray(o)) return o;
  const owner = o[0];
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
  const [n, g] = t.delete(owner, doomed);
  return { pruned: n, grants_removed: g };
};

const inject: Verb = (c, payload) => {
  const who = callerOf(c);
  if (!Array.isArray(who)) return who;
  if (!isAdmin(c, who[1])) {
    return refusal(UNAUTHORIZED, `inject: '${who[1]}' is not an admin of ${c.agent.id}`, { extra: { side: "store" } });
  }
  const t = loadTables(payload["tables"]);
  if (!(t instanceof Tables)) return t;
  TABLES.set(c.agent, t);
  let rows = 0;
  for (const m of t.rows.values()) rows += m.size;
  return { rows, grants: t.grants.size };
};

const dump: Verb = (c) => {
  const who = callerOf(c);
  if (!Array.isArray(who)) return who;
  if (!isAdmin(c, who[1])) {
    return refusal(UNAUTHORIZED, `dump: '${who[1]}' is not an admin of ${c.agent.id}`, { extra: { side: "store" } });
  }
  return { tables: tablesOf(c.agent).dump() };
};

/** Verb → one-line doc. The kernel answers `reflect` for every agent (no bundle
 *  can), so these reach a reader through the bundle readme below. */
export const RAM_STORE_VERBS: { [verb: string]: string } = {
  boot: "No-op — the tables start empty (RAM) or are filled by `inject`.",
  append:
    "args: rows:list[dict] (req), owner:str? (admins only). Appends rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}.",
  read:
    "args: grant:int? (read another owner's scope), owner:str? (admins), after:int?, before:int?, limit:int?, order:'asc'|'desc'?, kinds:list?, match:dict? (exact field equality), ids:list?, from_last_kind:str? (start at the newest row of that kind). Rows whole, oldest first by default. Returns {rows, next, total}.",
  search:
    "args: query:str (req — case-insensitive substring), grant:int?, owner:str? (admins), ids:list? (rows to search; a fold id includes its members), limit:int? (default 100), width:int? (chars per line, default 200). Every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}. (optional verb)",
  share:
    "args: with:str (req — the reader's agent id), scope:dict (req — ONE of {head_until:<row>}, {range:{from,to}}, {fold:<id>}, {row:<id>}), mode:'frozen'|'live'? (default frozen), why:str?. Grants `with` read access to that scope of YOUR rows — only rows you own (never a foreign row, never one shared with you). An identical share returns the existing grant. Returns {grant, anchor, as_of, created}.",
  unshare: "args: grant:int (req). Revokes a grant YOU gave (admins: any). The reader's next read under it fails. Returns {unshared:true, grant}.",
  grants:
    "args: side:'given'|'received' (req). The grants YOU gave, or the ones given TO you. Returns {grants:[{grant, owner, reader, scope, mode, as_of, until, why, created_by, ts}]}.",
  prune:
    "args: before_id:int? and/or ids:list? (req — one of them), keep:list? (kinds kept), owner:str? (admins). Deletes YOUR rows below `before_id` (or exactly `ids`) except kinds in `keep`; every fold ref and every grant anchored on a deleted row goes with it (FK cascade). Returns {pruned, grants_removed}.",
  inject:
    "args: tables:dict (req — a `dump`). ADMINS ONLY. Replaces the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows, grants}.",
  dump:
    "No args. ADMINS ONLY. The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}], grants:[…]} — exactly what `inject` takes back. Returns {tables}.",
};

const VERBS: { [verb: string]: Verb } = {
  boot: () => null,
  append,
  read,
  search,
  share,
  unshare,
  grants,
  prune,
  inject,
  dump,
};

export const RAM_STORE_README =
  "Memory store in RAM (`ram_store.ts`) — rows per owner (the caller), folds, grants, search; " +
  "ephemeral: everything is lost when the page unloads (durable: false). Multi-tenant: every row " +
  "is keyed `<kernel guid>|<caller id>` (the operator is `kernel`); a caller reads and writes only " +
  "its own key plus what a grant covers. Record fields: `operator: true` serves the operator at " +
  "all; `admins: [ids]` (`kernel` = the operator) may name another `owner` and `inject`/`dump`. " +
  "Optional verbs: search. The root's `memory_store_id` and every AI agent's name one.\n\nVerbs:\n" +
  Object.entries(RAM_STORE_VERBS).map(([v, d]) => `- ${v}: ${d}`).join("\n");

export async function handler(id: string, payload: Payload, kernel: Kernel): Promise<Json> {
  const verb = payload["type"];
  const fn = typeof verb === "string" ? VERBS[verb] : undefined;
  if (fn === undefined) {
    return refusal(UNHANDLED, `ram_store: unknown type ${JSON.stringify(verb)}`, { extra: { verb: String(verb) } });
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
