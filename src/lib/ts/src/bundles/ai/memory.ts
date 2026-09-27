import type { Kernel } from "../../kernel/kernel.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import { nowSeconds } from "../../kernel/timers.ts";
import type { Provider } from "./seam.ts";

// THE AI AGENT'S MEMORY STORE — port of the memory-store parts of python
// `ai_core/core.py` (this bundle owns no rows and no files).
//
// An AI agent's conversation lives in the MEMORY STORE its record names
// (`memory_store_id`) — a separate agent (`ram_store.ts`) that keys every row by
// WHO WROTE IT. So this agent reads and writes ITS OWN rows only, by plain gated
// sends: the creator wires `can_call` to the store and the store's `callable_by`
// to this agent, explicitly. No store named ⇒ the agent fails loud at boot and at
// its first turn; a store that refuses or is unreachable mid-turn ⇒ the turn
// fails fast. Nothing falls back to RAM.
//
// THE HEAD CACHE. The rows from the last `clear` on (the head) are read ONCE, on
// the first turn after boot (boot is LAZY — it never calls the store), then kept
// in RAM WRITE-THROUGH: every row is appended to the store FIRST and enters the
// cache only when the store took it. The store is the truth: each turn-start
// read of the head reads the rows AFTER the newest cached id and merges them by
// id (a row another writer appended, an admin plant), a `clear` among them
// restarting the head.
//
// BODIES ARE NEVER PERSISTED. A reply over the inline cap is stored as its
// one-line `origin`; the body is held in RAM under its pid for THIS incarnation
// only (`grep` reads it) — after a restart there are no bodies, the model re-reads.

export class MemoryStoreError extends Error {}

export type Row = Payload & { id?: number };

/** This bundle's RAM for ONE kernel — kept on the kernel, so two kernels in one
 *  page (tests) never share it. All of it is scratch: reconstructible from the
 *  store, never persisted. */
/** One user submission waiting for, or holding, the agent's FIFO lock. */
export interface QueueEntry {
  client_id: string;
  text: string;
  send_id: string;
  queued_at: number;
}

/** The generation that holds the lock: the entry plus its phase, the text
 *  streamed so far and the kernel task it runs as (what `cancel` addresses). */
export interface CurrentEntry extends QueueEntry {
  started_at: number;
  phase: string;
  text_so_far: string;
  task_id: number | null;
  last_tool?: Json;
}

/** A per-agent FIFO serializer: one generation at a time; callers queue. */
export class FifoLock {
  locked = false;
  private readonly waiters: (() => void)[] = [];

  acquire(): Promise<void> {
    if (!this.locked) {
      this.locked = true;
      return Promise.resolve();
    }
    return new Promise((res) => this.waiters.push(res));
  }

  release(): void {
    const next = this.waiters.shift();
    if (next !== undefined) next(); // hand the lock over, still held
    else this.locked = false;
  }
}

export interface AIMem {
  heads: Map<string, Row[]>;
  grants: Map<string, Payload[]>;
  /** agent → {watcher id → wants bodies} */
  watchers: Map<string, Map<string, boolean>>;
  /** agent → {pid → a reply body held this run} */
  bodies: Map<string, Map<number, string>>;
  /** agent → the last projection, as `context_status` reports it */
  projection: Map<string, Payload>;
  /** agent → where the last compaction's reaction scan starts (a head index) */
  compactionMark: Map<string, number>;
  /** agent → where the last assembly took its view from */
  viewSource: Map<string, string>;
  /** agents whose next turn opens with `context_recall` */
  recallDue: Set<string>;
  /** agent → fold rows written since the last recall */
  foldsSince: Map<string, number>;
  /** agent → reported prompt tokens ÷ our estimate of what was sent */
  calib: Map<string, number>;
  /** agent → the endpoint's last reported prompt size */
  reported: Map<string, number>;
  /** agent → submissions waiting on its lock, in arrival order */
  queue: Map<string, QueueEntry[]>;
  /** agent → the generation holding its lock (present ⇔ in flight) */
  current: Map<string, CurrentEntry>;
  locks: Map<string, FifoLock>;
  /** agent → its INCARNATION, bumped on every boot: a reply to a call an earlier
   *  incarnation made is recorded, never acted on */
  incarnation: Map<string, number>;
  /** agent → its routing batches still settling (what `await` asks about) */
  inflight: Map<string, number>;
  /** agent → its provider, built once */
  providers: Map<string, Provider>;
}

export function aiMem(kernel: Kernel): AIMem {
  let m = kernel.bundleMem.get("ai_backend") as AIMem | undefined;
  if (m === undefined) {
    m = {
      heads: new Map(),
      grants: new Map(),
      watchers: new Map(),
      bodies: new Map(),
      projection: new Map(),
      compactionMark: new Map(),
      viewSource: new Map(),
      recallDue: new Set(),
      foldsSince: new Map(),
      calib: new Map(),
      reported: new Map(),
      queue: new Map(),
      current: new Map(),
      locks: new Map(),
      incarnation: new Map(),
      inflight: new Map(),
      providers: new Map(),
    };
    kernel.bundleMem.set("ai_backend", m);
  }
  return m;
}

function isErr(res: Json | undefined): res is Payload {
  return (
    res !== null && typeof res === "object" && !Array.isArray(res) &&
    Boolean((res as Payload)["error"])
  );
}

export function asId(x: unknown): number | null {
  if (typeof x === "number") return Number.isInteger(x) && x > 0 ? x : null;
  if (typeof x === "string" && /^\d+$/.test(x.trim())) {
    const n = Number(x.trim());
    return n > 0 ? n : null;
  }
  return null;
}

/** The store this agent's record names, or MemoryStoreError. */
export function storeOf(selfId: string, kernel: Kernel): string {
  const sid = kernel.get(selfId)?.meta["memory_store_id"];
  if (typeof sid !== "string" || sid === "") {
    throw new MemoryStoreError(
      `${selfId}: no \`memory_store_id\` — an AI agent keeps its conversation in a memory ` +
        `store and runs without none. Create one (e.g. a ram_store), set memory_store_id on ` +
        `${selfId}, and grant ${selfId} can_call on it AND the store callable_by for it`,
    );
  }
  return sid;
}

/** ONE gated send to this agent's memory store, AS ITSELF — the store keys a row
 *  by its writer, and this is this agent's own memory even when the code runs
 *  outside its own dispatch. A refusal or a failed dispatch is a MemoryStoreError. */
export async function storeCall(selfId: string, kernel: Kernel, payload: Payload): Promise<Payload> {
  const sid = storeOf(selfId, kernel);
  const view = kernel.currentOrigin === selfId ? kernel : kernel.scopedFor(selfId, kernel.currentTask);
  const out = (await view.send([{ target: sid, payload }]))[0]!;
  const res = out.result;
  if (out.status !== "completed" || isErr(res)) {
    const err = isErr(res) ? String(res["error"]) : out.status;
    const hint = isErr(res) && typeof res["hint"] === "string" ? ` (hint: ${res["hint"]})` : "";
    throw new MemoryStoreError(`${selfId}: memory store '${sid}' ${String(payload["type"])} failed: ${err}${hint}`);
  }
  return res !== null && typeof res === "object" && !Array.isArray(res) ? res : {};
}

/** A new incarnation reads its store afresh (the `boot` verb). */
export function resetIncarnation(selfId: string, kernel: Kernel): void {
  const m = aiMem(kernel);
  m.heads.delete(selfId);
  m.grants.delete(selfId);
}

// The store holds this agent's conversation AND its view rows. The CONVERSATION a
// reader sees is everything but the bookkeeping (an allow-list of kinds to HIDE).
export const BOOKKEEPING: ReadonlySet<string> = new Set(["view"]);
const TURN_ROLES = new Set(["user", "assistant", "system"]);

/** The head cache — loaded from the store on first use (lazy boot). */
export async function head(selfId: string, kernel: Kernel): Promise<Row[]> {
  const heads = aiMem(kernel).heads;
  let h = heads.get(selfId);
  if (h === undefined) {
    const res = await storeCall(selfId, kernel, { type: "read", from_last_kind: "clear" });
    h = ((res["rows"] as Row[] | undefined) ?? []).map((r) => ({ ...r }));
    heads.set(selfId, h);
  }
  return h;
}

/** THE STORE IS THE TRUTH; the head cache must never diverge from it. Reads the
 *  rows AFTER the newest cached id and merges them by id; a `clear` among them
 *  restarts the head. One small read per turn; the store stays passive. */
async function refreshHead(selfId: string, kernel: Kernel): Promise<Row[]> {
  const heads = aiMem(kernel).heads;
  const h = heads.get(selfId);
  const ids = (h ?? []).map((r) => r.id).filter((x): x is number => typeof x === "number");
  if (h === undefined || ids.length === 0) {
    heads.delete(selfId);
    return head(selfId, kernel);
  }
  const res = await storeCall(selfId, kernel, { type: "read", after: Math.max(...ids) });
  const seen = new Set(ids);
  for (const r of (res["rows"] as Row[] | undefined) ?? []) {
    if (seen.has(r.id as number)) continue;
    if (r["kind"] === "clear") h.length = 0;
    h.push({ ...r });
  }
  return h;
}

/**
 * The agent's conversation, in order. AGENT ID IS CONVERSATION ID: a second
 * conversation is a second agent. A CLEAR IS A BOUNDARY, NOT A DELETION: the head
 * starts at the LAST `clear` row; `behind` reads the WHOLE conversation (recall,
 * grep, `history {fold}` — the verbs that reach what is not in view).
 *
 * THE CONVERSATION IS ITS TURNS — rows with a chat `role`. A row the agent
 * `append`s straight to its store (a note to itself) is memory, not a turn.
 *
 * A `fork` row (`fork_from`) stands where another owner's granted rows are READ
 * back each time, under the grant — so a revoked grant fails this read.
 */
export async function loadHistory(
  selfId: string,
  kernel: Kernel,
  opts: { behind?: boolean } = {},
): Promise<Row[]> {
  let rows: Row[];
  if (opts.behind === true) {
    const res = await storeCall(selfId, kernel, { type: "read" });
    rows = (res["rows"] as Row[] | undefined) ?? [];
  } else {
    rows = [...(await refreshHead(selfId, kernel))];
  }
  rows = rows.filter(
    (r) => !BOOKKEEPING.has(String(r["kind"])) && TURN_ROLES.has(String(r["role"])),
  );
  if (rows.some((r) => r["kind"] === "fork")) rows = await expandForks(selfId, kernel, rows);
  return rows;
}

/** APPEND rows to the memory store, then to the head cache (write-through). A row
 *  without an id takes the kernel's next one — minted AT THE APPEND, so id order
 *  is append order. A `clear` restarts the head. Returns the stored rows. */
export async function saveHistory(selfId: string, kernel: Kernel, turns: Row[]): Promise<Row[]> {
  const rows: Row[] = turns.map((t) => (t.id === undefined ? { ...t, id: kernel.mintId() } : { ...t }));
  if (rows.length === 0) return rows;
  await storeCall(selfId, kernel, { type: "append", rows });
  const h = aiMem(kernel).heads.get(selfId);
  for (const r of rows) {
    if (h !== undefined) {
      if (r["kind"] === "clear") h.length = 0;
      h.push(r);
    }
    if (!BOOKKEEPING.has(String(r["kind"]))) announceRow(selfId, kernel, r);
  }
  return rows;
}

// ─── the view array: the agent's STATE ───────────────────────────────────

/** The ordered ids the agent sees, each at a resolution — a `view` row in its
 *  memory store (bookkeeping, never a turn); the newest one in the head is current. */
export async function saveView(
  selfId: string,
  kernel: Kernel,
  ids: { id: number; res: string }[],
  opts: { budget: number; why: string },
): Promise<void> {
  await saveHistory(selfId, kernel, [
    { kind: "view", v: 1, ts: nowSeconds(), budget: opts.budget, why: opts.why, ids: ids as unknown as Json },
  ]);
}

export async function loadView(selfId: string, kernel: Kernel): Promise<Row | null> {
  const h = await head(selfId, kernel);
  for (let i = h.length - 1; i >= 0; i--) {
    const r = h[i] as Row;
    if (r["kind"] === "view") return Array.isArray(r["ids"]) ? r : null;
  }
  return null;
}

// ─── reply bodies held in RAM (never persisted) ──────────────────────────

/** A reply too big to hand the model inline: kept IN RAM under its pid for this
 *  incarnation only; the model is handed its digest + `origin` line. */
export function storeReply(selfId: string, kernel: Kernel, pid: number | null, body: string): boolean {
  if (pid === null) return false;
  const bodies = aiMem(kernel).bodies;
  let mine = bodies.get(selfId);
  if (mine === undefined) bodies.set(selfId, (mine = new Map()));
  mine.set(pid, body);
  return true;
}

// ─── sharing: fork rows, grants, and the row event ───────────────────────
//
// CONTEXT IS SHARED BY GRANT, IN THE STORE. A model shares a scope of its OWN
// rows with another agent by calling its store's `share` (an explicit choice,
// recorded); the reader starts from it with `fork_from`, which appends ONE `fork`
// row naming the grant. That row is rendered, every time the conversation is
// read, as the owner's rows READ BACK UNDER THE GRANT — quoted, attributed,
// read-only, never the reader's own turns. A revoked or pruned grant fails that
// read: fail fast.

export function forkFraming(owner: string, asOf: number): string {
  return (
    `Above is ${owner}'s thread as of row ${asOf}, shared read-only. This is a fork: ` +
    `you are a new agent, not ${owner}.`
  );
}

/** The owner's granted rows as ONE quoted block. A row that is itself a `fork`
 *  in the owner's thread came from a THIRD agent's grant the reader does not
 *  hold: it stands as a stub in its place — position kept, no content — naming
 *  the THIRD owner's own row (`as_of`), the one `request_share` to it can name. */
export function renderShared(owner: string, asOf: Json, rows: Row[]): string {
  const lines = [`[shared from ${owner} · read-only · as of row ${String(asOf)}]`];
  for (const r of rows) {
    if (r["kind"] === "view") continue;
    if (r["kind"] === "fork") {
      lines.push(`[row ${String(r["as_of"])} · owner ${String(r["owner"])} · not shared with you]`);
      continue;
    }
    const c = r["content"];
    const text = typeof c === "string" ? c : JSON.stringify(c ?? null);
    lines.push(`(${String(r["role"] ?? "user")} · row ${String(r.id)}) ${text}`);
  }
  lines.push(`[end of ${owner}'s shared rows]`);
  return lines.join("\n");
}

async function expandForks(selfId: string, kernel: Kernel, rows: Row[]): Promise<Row[]> {
  const out: Row[] = [];
  for (const r of rows) {
    if (r["kind"] !== "fork") {
      out.push(r);
      continue;
    }
    const res = await storeCall(selfId, kernel, { type: "read", grant: r["grant"] ?? null });
    const shared = (res["rows"] as Row[] | undefined) ?? [];
    const headLine = String(r["content"] ?? "").split("\n", 1)[0] as string;
    out.push({
      ...r,
      content: `${headLine}\n${renderShared(String(r["owner"]), r["as_of"] ?? null, shared)}`,
    });
  }
  return out;
}

/** The grants this agent GAVE, cached write-through (refreshed after the model
 *  calls its store's share/unshare). */
export async function myGrants(selfId: string, kernel: Kernel): Promise<Payload[]> {
  const cache = aiMem(kernel).grants;
  let g = cache.get(selfId);
  if (g === undefined) {
    const res = await storeCall(selfId, kernel, { type: "grants", side: "given" });
    g = (res["grants"] as Payload[] | undefined) ?? [];
    cache.set(selfId, g);
  }
  return g;
}

/** Drop and reload the grants cache (a share/unshare just changed it). */
export async function refreshGrants(selfId: string, kernel: Kernel): Promise<void> {
  aiMem(kernel).grants.delete(selfId);
  try {
    await myGrants(selfId, kernel);
  } catch (e) {
    if (!(e instanceof MemoryStoreError)) throw e;
    // the next body event simply carries none — no grant is assumed
  }
}

export function grantCovers(g: Payload, rid: number, rows: Row[]): boolean {
  const scope = (g["scope"] as Payload | undefined) ?? {};
  const live = g["mode"] === "live";
  const asOf = g["as_of"];
  if (!live && typeof asOf === "number" && rid > asOf) return false;
  if ("row" in scope) return rid === scope["row"];
  if ("range" in scope) {
    const r = scope["range"] as Payload;
    return rid >= (r["from"] as number) && (live || rid <= (r["to"] as number));
  }
  if ("head_until" in scope) return live || rid <= (scope["head_until"] as number);
  if ("fold" in scope) {
    const byId = new Map(rows.map((r) => [r.id, r]));
    const seen = new Set<number>();
    const stack = [scope["fold"] as number];
    while (stack.length > 0) {
      const x = stack.pop() as number;
      if (seen.has(x)) continue;
      seen.add(x);
      const refs = byId.get(x)?.["refs"];
      if (Array.isArray(refs)) stack.push(...(refs as number[]));
    }
    return seen.has(rid);
  }
  return false;
}

// THE ROW EVENT — the kernel holds no rows, so this agent announces its own:
// {watcher id → wants bodies}, registered by the `watch` verb (the agent's own
// gate decides who may call it); a watcher whose inbox is gone is dropped.

export function addWatcher(selfId: string, kernel: Kernel, watcher: string, body: boolean): void {
  const m = aiMem(kernel).watchers;
  let ws = m.get(selfId);
  if (ws === undefined) m.set(selfId, (ws = new Map()));
  ws.set(watcher, body);
}

export function removeWatcher(selfId: string, kernel: Kernel, watcher: string): void {
  aiMem(kernel).watchers.get(selfId)?.delete(watcher);
}

function rowDigest(row: Row): Payload {
  const c = row["content"];
  const text = typeof c === "string" ? c : JSON.stringify(c ?? null);
  return {
    id: row.id ?? null,
    kind: (row["kind"] as Json | undefined) ?? (row["role"] as Json | undefined) ?? null,
    role: (row["role"] as Json | undefined) ?? null,
    bytes: text.length,
  };
}

/** Put the row event in every watcher's inbox: a content-free digest, plus the
 *  content for a `body:true` watcher holding a live grant covering the row (the
 *  check uses the cached grants — see `myGrants`). */
function announceRow(selfId: string, kernel: Kernel, row: Row): void {
  const m = aiMem(kernel);
  const ws = m.watchers.get(selfId);
  if (ws === undefined || ws.size === 0) return;
  const grants = m.grants.get(selfId) ?? [];
  const h = m.heads.get(selfId) ?? [];
  for (const [wid, body] of [...ws]) {
    if (!kernel.hasInbox(wid)) {
      ws.delete(wid);
      continue;
    }
    const digest = rowDigest(row);
    if (body && typeof row.id === "number" &&
      grants.some((g) => g["reader"] === wid && grantCovers(g, row.id as number, h))) {
      digest["content"] = row["content"] ?? null;
    }
    kernel.deliverInbox(wid, { type: "row", source: selfId, row: digest });
  }
}
