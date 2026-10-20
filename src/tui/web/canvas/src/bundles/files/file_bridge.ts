import { Host } from "../../lib.ts";
import type { Json, Kernel, Payload } from "../../lib.ts";
import { norm, parentOf, under, wire } from "./paths.ts";
import { WatchLedger } from "./watch_ledger.ts";

// THE PAGE'S CLIENT of the project's file bridge — the host agent `files`
// (`file_bridge.tools`, root `.`): list, read, read_stream, write, undo and the
// live `watch_path`. The bridge's protocol is used as it is; nothing here
// changes it.

export const FILE_BRIDGE = "files";

/** What the tree and the previewer call. `edit` is granted as the issue lists it,
 *  but a save is always a whole-document `write` with `if_match`: a SEARCH/REPLACE
 *  needs a unique SEARCH span, which a computed diff cannot guarantee. */
export const FILE_VERBS = ["list", "read", "read_stream", "watch_path", "unwatch_path", "write", "edit", "undo", "watch"] as const;

/** A cell's `can_call` onto the bridge (a fresh copy each time). */
export const fileGrant = (): Record<string, string[]> => ({ [FILE_BRIDGE]: [...FILE_VERBS] });

export interface FileBridge {
  call(p: Record<string, unknown>): Promise<Json>;
}

/** The real bridge: the page's operator calling `files` on the host. */
export const kernelBridge = (kernel: Kernel): FileBridge => ({ call: (p) => kernel.callHost(FILE_BRIDGE, p as Payload) });

export type Refusal = Record<string, unknown> & { error: string };

export const isRefusal = (r: unknown): r is Refusal =>
  r !== null && typeof r === "object" && typeof (r as Record<string, unknown>)["error"] === "string";

/** A refusal's one line for the screen. */
export const said = (r: Refusal): string => `${r.error}${typeof r["hint"] === "string" ? ` — ${r["hint"]}` : ""}`;

/** Raw bytes from a `read_stream` chunk (a binary frame arrives as bytes). */
export function bytesOf(raw: unknown): Uint8Array | null {
  if (raw instanceof Uint8Array) return raw;
  if (raw instanceof ArrayBuffer) return new Uint8Array(raw);
  if (ArrayBuffer.isView(raw)) return new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength);
  if (typeof raw === "string") return new TextEncoder().encode(raw);
  return null;
}

export type Read = { ok: true; text: string; version: string } | { ok: false; refusal: Refusal };

const PAGE_LINES = 2000;
const TRIES = 3;

/** Whether the file's last byte is a newline (`read` drops the phantom empty last line). */
async function endsWithNewline(b: FileBridge, path: string): Promise<boolean | Refusal> {
  const head = (await b.call({ type: "read_stream", path: wire(path), offset: 0, length: 1 })) as Record<string, unknown>;
  if (isRefusal(head)) return head;
  const size = Number(head["size"]);
  if (!(size > 0)) return false;
  const tail = (await b.call({ type: "read_stream", path: wire(path), offset: size - 1, length: 1 })) as Record<string, unknown>;
  if (isRefusal(tail)) return tail;
  return bytesOf(tail["bytes"])?.[0] === 10;
}

/** The whole text file, paged. The `version` must be one on every page: a file that
 *  changes mid-read is read again, up to `TRIES` times, then said so. */
export async function readAll(b: FileBridge, path: string): Promise<Read> {
  for (let attempt = 0; attempt < TRIES; attempt++) {
    const lines: string[] = [];
    let version: string | null = null;
    let changed = false;
    for (let offset = 0; ; ) {
      const r = (await b.call({ type: "read", path: wire(path), offset, limit: PAGE_LINES })) as Record<string, unknown>;
      if (isRefusal(r)) return { ok: false, refusal: r };
      const v = String(r["version"] ?? "");
      if (version !== null && v !== version) {
        changed = true;
        break;
      }
      version = v;
      const page = Array.isArray(r["lines"]) ? (r["lines"] as Array<{ text?: unknown }>) : [];
      for (const l of page) lines.push(String(l.text ?? ""));
      if (r["eof"] === true || page.length === 0) break;
      offset += page.length;
    }
    if (changed) continue;
    const nl = lines.length > 0 ? await endsWithNewline(b, path) : false;
    if (isRefusal(nl)) return { ok: false, refusal: nl };
    return { ok: true, text: lines.join("\n") + (nl && lines.length > 0 ? "\n" : ""), version: version ?? "" };
  }
  return { ok: false, refusal: { error: `${path} kept changing while it was read`, reason: "changing", hint: "read it again once the writer is done" } };
}

export type Bytes = { ok: true; bytes: Uint8Array } | { ok: false; refusal: Refusal };

/** A whole file's bytes through `read_stream`, up to `max` (a larger file is refused, said). */
export async function readBytes(b: FileBridge, path: string, max: number): Promise<Bytes> {
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (let offset = 0; ; ) {
    const r = (await b.call({ type: "read_stream", path: wire(path), offset, length: 1 << 20 })) as Record<string, unknown>;
    if (isRefusal(r)) return { ok: false, refusal: r };
    const size = Number(r["size"]);
    if (size > max) return { ok: false, refusal: { error: `${path} is ${size} bytes; the previewer reads at most ${max}`, reason: "too_large" } };
    const chunk = bytesOf(r["bytes"]);
    if (chunk === null) return { ok: false, refusal: { error: `read_stream ${path}: the chunk is not bytes`, reason: "bad_reply" } };
    chunks.push(chunk);
    total += chunk.length;
    if (r["eof"] === true || chunk.length === 0) break;
    offset = Number(r["next_offset"]);
  }
  const out = new Uint8Array(total);
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return { ok: true, bytes: out };
}

// ─── WATCHES, ref-counted for the whole page ────────────────────────────────

export interface Change {
  path: string;
  kind: string;
}

/** The bridge's `changed` events, as a subscription (and how to end it). */
export type Subscribe = (fn: (ev: Record<string, unknown>) => void) => () => void;

export class WatchHub {
  private readonly ledger = new WatchLedger();
  private readonly subs = new Map<string, Map<string, (c: Change) => void>>();
  private readonly arming = new Map<string, Promise<void>>();
  private readonly pending = new Map<string, Change>();
  private off: (() => void) | null = null;

  private readonly bridge: FileBridge;
  private readonly subscribe: Subscribe;

  constructor(bridge: FileBridge, subscribe: Subscribe) {
    this.bridge = bridge;
    this.subscribe = subscribe;
  }

  /** Every path held on the bridge by this page. */
  held(): string[] {
    return this.ledger.held();
  }

  /** `owner` follows `path`: a folder reports its entries, a file reports itself. A refusal
   *  (the bridge's, or no such path) is thrown, with the hold already let go. */
  async hold(path: string, owner: string, fn: (c: Change) => void): Promise<void> {
    const p = norm(path);
    let mine = this.subs.get(owner);
    if (mine === undefined) this.subs.set(owner, (mine = new Map()));
    mine.set(p, fn);
    this.off ??= this.subscribe((ev) => this.onEvent(ev));
    const act = this.ledger.acquire(p, owner);
    try {
      if (act === "watch") {
        const arm = this.arm(p);
        this.arming.set(p, arm);
        await arm;
      } else {
        await this.arming.get(p); // another cell is arming it: share its outcome
      }
    } catch (e) {
      this.unhold(p, owner, false); // nothing was armed: nothing to tell the bridge
      throw e;
    } finally {
      if (act === "watch") this.arming.delete(p);
    }
  }

  private async arm(p: string): Promise<void> {
    const r = await this.bridge.call({ type: "watch_path", path: wire(p) });
    if (isRefusal(r)) throw new Error(`watch ${p || "."}: ${said(r)}`);
  }

  /** Remove `owner`'s hold on `p` from the books; the bridge is told when it was the last. */
  private unhold(p: string, owner: string, tell: boolean): void {
    this.subs.get(owner)?.delete(p);
    if (this.subs.get(owner)?.size === 0) this.subs.delete(owner);
    if (this.ledger.release(p, owner) === "unwatch" && tell) void this.disarm(p);
    this.settle();
  }

  private async disarm(p: string): Promise<void> {
    const r = await this.bridge.call({ type: "unwatch_path", path: wire(p) });
    if (isRefusal(r)) console.error(`[files] unwatch ${p || "."}: ${said(r)}`);
  }

  /** The events source ends with the last hold. */
  private settle(): void {
    if (this.ledger.held().length === 0 && this.off !== null) {
      this.off();
      this.off = null;
    }
  }

  drop(path: string, owner: string): void {
    this.unhold(norm(path), owner, true);
  }

  dropAll(owner: string): void {
    for (const p of this.ledger.releaseAll(owner)) void this.disarm(p);
    this.subs.delete(owner);
    this.settle();
  }

  private onEvent(ev: Record<string, unknown>): void {
    if (typeof ev["path"] !== "string") return;
    const path = norm(ev["path"]);
    // events for one path within a tick are one: the last kind stands
    const first = this.pending.size === 0;
    this.pending.set(path, { path, kind: String(ev["kind"] ?? "modified") });
    if (first) queueMicrotask(() => this.flush());
  }

  private flush(): void {
    const batch = [...this.pending.values()];
    this.pending.clear();
    for (const c of batch) {
      const parent = parentOf(c.path);
      for (const mine of [...this.subs.values()]) {
        for (const [held, fn] of mine) {
          if (held !== c.path && held !== parent) continue;
          try {
            fn(c);
          } catch (e) {
            console.error("[files] a change handler threw:", e);
          }
        }
      }
    }
  }
}

// ─── THE FILE AND FOLDER AGENTS (#150) ──────────────────────────────────────
// A path a VIEW AGENT serves (`file_bridge/views.py`) reaches the disk through that
// agent: a file's text reads and writes, a folder's listing, and the watch whose
// changes the agent relays onto this page. Every other verb, and every other path,
// goes to the bridge as it always did. The routing is per path, so the page keeps
// ONE hub, one ledger and one arm per path.

export type ViewKind = "file" | "folder";

/** A path a view agent serves on this page. */
export interface Served {
  view: string;
  kind: ViewKind;
  path: string;
}

interface ViewSet {
  served: Map<string, Served>;
  /** who holds each served path: the cells bound to it. A path is served while any holds it. */
  holders: Map<string, Set<string>>;
  listeners: Set<(ev: Record<string, unknown>) => void>;
  /** the sources this page listens to for `changed` */
  watching: Set<string>;
  host: Host;
}

const viewSets = new WeakMap<Kernel, ViewSet>();

function viewSet(kernel: Kernel): ViewSet {
  let s = viewSets.get(kernel);
  if (s === undefined) {
    s = {
      served: new Map(),
      holders: new Map(),
      listeners: new Set(),
      watching: new Set(),
      host: new Host(kernel, "files_hub"),
    };
    viewSets.set(kernel, s);
  }
  return s;
}

/** The state stream's removals: a view that is deleted stops serving its paths. */
export function onStateFrame(kernel: Kernel, frame: Record<string, unknown>): void {
  if (frame["kind"] === "removed" && typeof frame["agent_id"] === "string") dropView(kernel, frame["agent_id"]);
}

/** What a verb sent to a served path becomes: the view it goes to and the payload it
 *  sends, or the answer the page gives itself (an unwatch: the view's relay lives with
 *  the view, and ends when the view is deleted, so there is nothing to tell it). */
type Route = { view: string; payload: Record<string, unknown> } | { local: Json };

/** The folder view that covers `p` most closely (the innermost), if any. */
function innermostFolder(served: Map<string, Served>, p: string): Served | undefined {
  let best: Served | undefined;
  for (const sv of served.values()) {
    if (sv.kind !== "folder" || !under(p, sv.path)) continue;
    if (best === undefined || sv.path.length > best.path.length) best = sv;
  }
  return best;
}

/** A folder view's operand: the path inside its folder (`.` for the folder itself). */
const insideFolder = (root: string, p: string): string => (p === root ? "." : root === "" ? p : p.slice(root.length + 1));

/** The route of one bridge call, or null when the bridge serves it. Pure: the tests read it. */
export function routeOf(s: Map<string, Served>, p: Record<string, unknown>): Route | null {
  const type = String(p["type"]);
  const raw = typeof p["path"] === "string" ? p["path"] : type === "list" ? "" : null;
  if (raw === null) return null;
  const path = norm(raw);
  switch (type) {
    case "read":
    case "write": {
      const f = s.get(path);
      return f?.kind === "file" ? { view: f.view, payload: { ...p, path } } : null;
    }
    case "list": {
      const f = innermostFolder(s, path);
      return f === undefined ? null : { view: f.view, payload: { ...p, path: insideFolder(f.path, path) } };
    }
    case "watch_path": {
      const sv = s.get(path);
      return sv === undefined ? null : { view: sv.view, payload: { type: "watch" } };
    }
    case "unwatch_path": {
      return s.has(path) ? { local: { watching: false, paths: [] } } : null;
    }
    default:
      return null;
  }
}

/** The page's bridge client that also knows its view agents: a served path goes to its
 *  view, everything else to the bridge. */
export function routedBridge(kernel: Kernel): FileBridge {
  return {
    call: async (p) => {
      const r = routeOf(viewSet(kernel).served, p);
      if (r === null) return kernelBridge(kernel).call(p);
      if ("local" in r) return r.local;
      return kernel.callHost(r.view, r.payload as Payload);
    },
  };
}

/** A view's event as a bridge `changed`: a folder's `added|changed|removed` is the
 *  bridge's `added|modified|deleted`, so one hub judges both the same way. */
export function changeOf(ev: Record<string, unknown>): Record<string, unknown> {
  if (typeof ev["kind"] === "string") return ev;
  const kind = ev["type"] === "added" ? "added" : ev["type"] === "removed" ? "deleted" : "modified";
  return { ...ev, kind };
}

/** Listen to `src`'s `changed` events, once, into the page's listeners. */
function listenTo(s: ViewSet, src: string): void {
  if (s.watching.has(src)) return;
  s.watching.add(src);
  s.host.on(src, "changed", (p) => {
    const ev = changeOf(p as Record<string, unknown>);
    for (const fn of [...s.listeners]) fn(ev);
  });
}

/** The `changed` events of the bridge and of every served view, as one subscription. */
function changesOf(kernel: Kernel): Subscribe {
  return (fn) => {
    const s = viewSet(kernel);
    s.listeners.add(fn);
    listenTo(s, FILE_BRIDGE);
    for (const sv of s.served.values()) listenTo(s, sv.view);
    return () => {
      s.listeners.delete(fn);
      if (s.listeners.size === 0) {
        for (const src of s.watching) s.host.unwatch(src);
        s.watching.clear();
      }
    };
  };
}

/** The page's served paths, for the tests of its routes and releases. */
export function servedOf(kernel: Kernel): Map<string, Served> {
  return viewSet(kernel).served;
}

/** Route `path` through view `view` on this page: its routes now, its events once
 *  anything listens. Holds nothing: a route is the page's answer, and a cell holds it. */
function route(kernel: Kernel, kind: ViewKind, path: string, view: string): void {
  const s = viewSet(kernel);
  const p = norm(path);
  s.served.set(p, { view, kind, path: p });
  if (s.listeners.size > 0) listenTo(s, view);
}

/** Serve `path` through view `view` for `holder` (a cell's id): the route holds until that
 *  holder releases it (`release`). */
export function serve(kernel: Kernel, kind: ViewKind, path: string, view: string, holder: string): void {
  route(kernel, kind, path, view);
  const s = viewSet(kernel);
  const p = norm(path);
  const held = s.holders.get(p) ?? new Set<string>();
  held.add(holder);
  s.holders.set(p, held);
}

/** `holder` lets go of `path`. The path stops being served when its last holder lets go;
 *  a holder that holds nothing here is a no-op. */
export function release(kernel: Kernel, path: string, holder: string): void {
  const s = viewSet(kernel);
  const p = norm(path);
  const held = s.holders.get(p);
  if (held === undefined || !held.delete(holder)) return;
  if (held.size > 0) return;
  s.holders.delete(p);
  const sv = s.served.get(p);
  s.served.delete(p);
  if (sv !== undefined && !viewIsServed(s, sv.view)) unlisten(s, sv.view);
}

/** A view is gone (deleted, or removed from the state): every path it serves is released,
 *  and its events are no longer listened to. */
export function dropView(kernel: Kernel, view: string): void {
  const s = viewSet(kernel);
  for (const [p, sv] of [...s.served]) {
    if (sv.view !== view) continue;
    s.served.delete(p);
    s.holders.delete(p);
  }
  unlisten(s, view);
}

function viewIsServed(s: ViewSet, view: string): boolean {
  for (const sv of s.served.values()) if (sv.view === view) return true;
  return false;
}

function unlisten(s: ViewSet, src: string): void {
  if (!s.watching.delete(src)) return;
  s.host.unwatch(src);
}

/** What the host's grant watch has said about each agent, on the state stream
 *  (`reach.rs` `announce`): `granted` once the page's grant is written, `grant_refused`
 *  with why it was not. The watch says it once per view, on the view's `added`; the page's
 *  calls and the state stream are separate tasks on the host, so a word may come before
 *  or after a call's reply. The board remembers each word, and the views still owed one. */
interface GrantBoard {
  /** the host's word on each view: null once granted, else why it was refused */
  readonly said: Map<string, Refusal | null>;
  /** the views this page made (`open_view` answered `created`): only for these is a word
   *  sure to come, and they are owed until it does */
  readonly owed: Set<string>;
  /** the page's calls listening for a view's word */
  readonly waiting: Map<string, Set<(r: Refusal | null) => void>>;
}

const boards = new WeakMap<Kernel, GrantBoard>();

/** The page's grant board, listening on the state stream from its first use. */
function grantBoard(kernel: Kernel): GrantBoard {
  const had = boards.get(kernel);
  if (had !== undefined) return had;
  const board: GrantBoard = { said: new Map(), owed: new Set(), waiting: new Map() };
  boards.set(kernel, board);
  kernel.subscribeState((frame) => {
    const id = frame["agent_id"];
    if (typeof id !== "string") return;
    const kind = frame["kind"];
    let verdict: Refusal | null;
    if (kind === "granted") verdict = null;
    else if (kind === "grant_refused") {
      const why = frame["error"];
      verdict = { error: typeof why === "string" ? why : `the host refused the grant on ${id}`, reason: "grant_refused" };
    } else if (kind === "removed") {
      // a view deleted: its word is forgotten, and one still owed will not come
      board.said.delete(id);
      if (!board.owed.has(id)) return;
      verdict = { error: `${id} was removed before the host granted it`, reason: "removed" };
    } else return;
    if (kind !== "removed") board.said.set(id, verdict);
    board.owed.delete(id);
    const waiters = board.waiting.get(id) ?? new Set();
    board.waiting.delete(id);
    for (const w of waiters) w(verdict);
  });
  return board;
}

/** The page waits for its grant on a view: `reflect` once, and when the host refuses it
 *  as a missing egress grant, the host's word on that view. Null once the page may reach
 *  the view; any other refusal is returned at once. The page listens BEFORE it probes, so
 *  a word that comes while the probe is out is heard, and one that came earlier is on the
 *  board. Only a view the host still owes a word for is waited on: a restored cell's view
 *  is never `added` again, so its refusal is returned. No timeout: the host always says
 *  its word on a view it made. */
export async function awaitGrant(kernel: Kernel, view: string): Promise<Refusal | null> {
  const board = grantBoard(kernel);
  const before = board.said.get(view);
  let heard: { verdict: Refusal | null } | undefined;
  let listen: (r: Refusal | null) => void = () => {};
  const word = new Promise<Refusal | null>((done) => {
    listen = (r) => {
      heard = { verdict: r };
      done(r);
    };
  });
  const mine = board.waiting.get(view) ?? new Set();
  mine.add(listen);
  board.waiting.set(view, mine);
  const stop = (): void => {
    mine.delete(listen);
    if (mine.size === 0 && board.waiting.get(view) === mine) board.waiting.delete(view);
  };
  const r = (await kernel.callHost(view, { type: "reflect" } as Payload)) as Record<string, unknown>;
  if (!isRefusal(r) || !(r["reason"] === "unauthorized" && r["side"] === "egress")) {
    stop();
    return isRefusal(r) ? r : null;
  }
  // the word came while the probe was out: it is newer than the probe's refusal
  if (heard !== undefined) return heard.verdict;
  // the word came before: a refusal says why; a grant the probe still met refused
  // leaves the probe's refusal standing
  if (before !== undefined) {
    stop();
    return before ?? r;
  }
  if (board.owed.has(view)) return word;
  stop();
  return r;
}

/** `open_view` on the bridge: the view agent of `path` — reused when one exists, made
 *  otherwise — served on this page. A refusal (the path is not exposed, say) is returned
 *  as it is. The page's grant on the view is awaited by the cell that reads it
 *  (`bindCellToView`), not here: this runs inside a handler, whose own grant decides. A
 *  view it made is owed the host's word on its grant; the board listens before the call,
 *  so a word that beats the reply is kept. */
export async function openViewAgent(
  kernel: Kernel,
  kind: ViewKind,
  path: string,
): Promise<{ view: string; created: boolean } | Refusal> {
  const board = grantBoard(kernel);
  const r = (await kernelBridge(kernel).call({ type: "open_view", kind, path: wire(norm(path)) })) as Record<string, unknown>;
  if (isRefusal(r)) return r;
  const id = r["id"];
  if (typeof id !== "string") return { error: `open_view ${path}: the reply names no view`, reason: "bad_reply" };
  if (r["created"] === true) board.owed.add(id);
  route(kernel, kind, path, id);
  return { view: id, created: r["created"] === true };
}

/** A cell's binding to the view of its path, at mount. A cell saved as a view carries its
 *  `view_agent`: that agent serves it again, as it did. A cell saved before views (none)
 *  gets the view of its path, made or reused, and `record` names it on the cell. Either
 *  way the page waits for its grant on the view (`awaitGrant`), so the cell's first reads
 *  are not refused in the gap; a saved view the host has no word to come on (a restored
 *  cell's, refused) is refused at once. A refusal is returned for the cell to show; the
 *  cell itself stays open, and its reads are the view's, so they are refused and shown too. */
export async function bindCellToView(
  kernel: Kernel,
  kind: ViewKind,
  path: string,
  saved: string | undefined,
  record: (view: string) => void,
  holder = "",
): Promise<{ ok: true; view: string } | { ok: false; refusal: Refusal }> {
  let view: string;
  if (saved !== undefined) {
    serve(kernel, kind, path, saved, holder);
    view = saved;
  } else {
    const r = await openViewAgent(kernel, kind, path);
    if (isRefusal(r)) return { ok: false, refusal: r };
    record(r.view);
    serve(kernel, kind, path, r.view, holder);
    view = r.view;
  }
  const ungranted = await awaitGrant(kernel, view);
  return ungranted === null ? { ok: true, view } : { ok: false, refusal: ungranted };
}

const hubs = new WeakMap<Kernel, WatchHub>();

/** The page's one hub: the bridge's changes and every served view's. */
export function sharedHub(kernel: Kernel): WatchHub {
  let h = hubs.get(kernel);
  if (h === undefined) {
    h = new WatchHub(routedBridge(kernel), changesOf(kernel));
    hubs.set(kernel, h);
  }
  return h;
}
