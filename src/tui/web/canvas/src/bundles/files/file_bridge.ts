import { Host } from "../../lib.ts";
import type { Json, Kernel, Payload } from "../../lib.ts";
import { norm, parentOf, wire } from "./paths.ts";
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

const hubs = new WeakMap<Kernel, WatchHub>();

/** The page's one hub. */
export function sharedHub(kernel: Kernel): WatchHub {
  let h = hubs.get(kernel);
  if (h === undefined) {
    const host = new Host(kernel, "files_hub");
    h = new WatchHub(kernelBridge(kernel), (fn) => {
      const off = host.on(FILE_BRIDGE, "changed", (p) => fn(p as Record<string, unknown>));
      return () => {
        off();
        host.unwatch(FILE_BRIDGE);
      };
    });
    hubs.set(kernel, h);
  }
  return h;
}
