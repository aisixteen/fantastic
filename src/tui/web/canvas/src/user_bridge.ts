// THE PAGE AS A USER (owner, 2026-10-03): every host call this tab makes goes
// through the host's USERS GATEWAY (`users`, `canvas_users.tools`) with this
// tab's token, and the gateway acts as the user that token is — the host's gate
// decides by that user's own grants.
//
//   · a call to host agent T     → `users.forward {target: T, payload, token, tab}`
//                                  (the item's marks re-rooted one `payload` down,
//                                  `payload.token` marked);
//   · a watch of host agent T    → `users.watch_agent {target: T, tab, token}`;
//   · the host state stream      → `users.state_subscribe {tab, token}`;
//   · what comes back            → ONE PULL LOOP, `users.events {tab, after}`:
//                                  `event` (a watched agent's), `state_event`, and
//                                  `record_changed` (another tab's accepted page
//                                  write, applied here; this tab's own is skipped).
//
// The token lives in browser storage only (never the URL); no token = the
// host's tokenless default. The tab's id is minted per load and kept nowhere.

import type { Bridge, Json, Kernel, Payload, WsBridge } from "./lib.ts";
import { Agent } from "./lib.ts";

type Mark = string | readonly (string | number)[];

/** The host's users gateway. */
export const GATEWAY = "users";
/** Where this browser keeps its token. */
export const TOKEN_KEY = "fantastic.canvas.token";

export function readToken(): string | null {
  try {
    const t = localStorage.getItem(TOKEN_KEY);
    return t !== null && t !== "" ? t : null;
  } catch {
    return null;
  }
}

export function writeToken(t: string | null): void {
  try {
    if (t === null || t === "") localStorage.removeItem(TOKEN_KEY);
    else localStorage.setItem(TOKEN_KEY, t);
  } catch {
    /* no storage: the token lives for this load only */
  }
}

/** The item's marks, re-rooted at the gateway's `forward` item: a mark under
 *  `payload` gains one `payload` step; a `result` mark is the same reply. */
export function rerooted(secret: readonly Mark[] | undefined, withToken: boolean): Mark[] {
  const out: Mark[] = [];
  for (const m of secret ?? []) {
    if (typeof m === "string") out.push(m === "result" || m.startsWith("result.") ? m : `payload.${m}`);
    else out.push(m[0] === "result" ? m : ["payload", ...m]);
  }
  if (withToken) out.push("payload.token");
  return out;
}

/** A record's JSON with its keys sorted, to tell an echo from a change. */
export function canon(v: unknown): string {
  return JSON.stringify(v, (_k, x: unknown) =>
    x !== null && typeof x === "object" && !Array.isArray(x)
      ? Object.fromEntries(Object.entries(x as Record<string, unknown>).sort(([a], [b]) => (a < b ? -1 : 1)))
      : x,
  );
}

export interface UserBridgeOptions {
  /** The host-side page loader (its records are the page tree). */
  loader: string;
  /** This tab's token, read at each call (a new one takes effect at once). */
  token: () => string | null;
  /** This tab's id, minted at load. */
  tab: string;
  /** The pull's longest wait, seconds. */
  wait?: number;
}

export class UserBridge implements Bridge {
  private readonly kernel: Kernel;
  private readonly inner: WsBridge;
  private readonly o: UserBridgeOptions;
  private readonly watched = new Set<string>();
  private readonly stateHandlers = new Set<(frame: Record<string, unknown>) => void>();
  /** page records as the last remote change set them: id → canon(record) | "removed" */
  private readonly applied = new Map<string, string>();
  /** the pull's cursor — null until the first pull, which starts at the head */
  private after: number | null = null;
  private running = false;

  constructor(kernel: Kernel, inner: WsBridge, o: UserBridgeOptions) {
    this.kernel = kernel;
    this.inner = inner;
    this.o = o;
    kernel.bridge = this;
    // the host restarted or the socket came back: the gateway forgot this tab
    inner.onLifecycle(GATEWAY, (s) => {
      if (s === "connected") void this.resubscribe();
    });
  }

  private wrap(target: string, payload: Payload | Record<string, unknown>): Record<string, unknown> {
    const token = this.o.token();
    if (target === GATEWAY) return { ...payload, ...(token !== null ? { token } : {}), tab: this.o.tab };
    const w: Record<string, unknown> = { type: "forward", target, payload, tab: this.o.tab };
    if (token !== null) w["token"] = token;
    return w;
  }

  private marks(target: string, secret?: readonly Mark[]): Mark[] {
    const token = this.o.token() !== null;
    if (target === GATEWAY) return [...(secret ?? []), ...(token ? ["payload.token"] : [])];
    return rerooted(secret, token);
  }

  /** A page-tree write that only repeats the last remote change: not sent. */
  private echo(target: string, payload: Payload | Record<string, unknown>): boolean {
    if (target !== this.o.loader) return false;
    const t = payload["type"];
    if (t === "persist_record") {
      const rec = payload["record"] as Record<string, unknown> | undefined;
      const id = typeof rec?.["id"] === "string" ? rec["id"] : "";
      return id !== "" && this.applied.get(id) === canon(rec);
    }
    if (t === "forget_record") return this.applied.get(String(payload["id"])) === "removed";
    return false;
  }

  forward(target: string, payload: Payload, caller?: string, secret?: readonly Mark[]): Promise<Json> {
    if (this.echo(target, payload)) return Promise.resolve({ ok: true, echo: true });
    return this.inner.forward(GATEWAY, this.wrap(target, payload) as Payload, caller, this.marks(target, secret) as never);
  }

  forwardBinary(target: string, payload: Record<string, unknown>, caller?: string, secret?: readonly Mark[]): Promise<Json> {
    return this.inner.forwardBinary(GATEWAY, this.wrap(target, payload), caller, this.marks(target, secret) as never);
  }

  /** A host agent's inbox, fire-and-forget: the user `emit`s to it. */
  emitRemote(target: string, payload: Payload): void {
    void this.forward(target, { ...payload });
  }

  watchRemote(src: string): void {
    this.watched.add(src);
    void this.gw({ type: "watch_agent", target: src });
    this.start();
  }

  unwatchRemote(src: string): void {
    this.watched.delete(src);
    void this.gw({ type: "unwatch_agent", target: src });
  }

  onLifecycle(_endpoint: string, fn: (state: "connected" | "disconnected") => void): () => void {
    return this.inner.onLifecycle(GATEWAY, fn);
  }

  subscribeState(handler: (frame: Record<string, unknown>) => void): () => void {
    const first = this.stateHandlers.size === 0;
    this.stateHandlers.add(handler);
    if (first) void this.subscribe();
    this.start();
    return () => {
      this.stateHandlers.delete(handler);
      if (this.stateHandlers.size === 0) void this.gw({ type: "state_unsubscribe" });
    };
  }

  private gw(payload: Record<string, unknown>): Promise<Json> {
    return this.forward(GATEWAY, payload as Payload).catch((e: unknown) => ({ error: String(e) }) as Json);
  }

  private async subscribe(): Promise<void> {
    const r = (await this.gw({ type: "state_subscribe" })) as Record<string, unknown> | null;
    if (r !== null && Array.isArray(r["agents"])) this.toState({ type: "state_snapshot", agents: r["agents"] });
  }

  private async resubscribe(): Promise<void> {
    for (const src of this.watched) await this.gw({ type: "watch_agent", target: src });
    if (this.stateHandlers.size > 0) await this.subscribe();
  }

  private toState(frame: Record<string, unknown>): void {
    for (const h of this.stateHandlers) {
      try {
        h(frame);
      } catch {
        /* keep dispatching */
      }
    }
  }

  /** THE PULL: one loop per tab, for as long as the page lives. */
  start(): void {
    if (this.running) return;
    this.running = true;
    void this.loop();
  }

  private async loop(): Promise<void> {
    while (this.running) {
      let r: Record<string, unknown> | null;
      try {
        r = (await this.forward(
          GATEWAY,
          (this.after === null
            ? { type: "events" }
            : { type: "events", after: this.after, wait: this.o.wait ?? 25 }) as Payload,
        )) as Record<string, unknown> | null;
      } catch {
        await new Promise((res) => setTimeout(res, 1000));
        continue;
      }
      if (r === null || typeof r["error"] === "string") {
        await new Promise((res) => setTimeout(res, 2000));
        continue;
      }
      if (r["lost"] === true) {
        // this tab fell behind the gateway's queue: its tree may be stale
        console.warn("canvas: events lost — reloading");
        location.reload();
        return;
      }
      for (const e of (r["events"] as Record<string, unknown>[] | undefined) ?? []) this.dispatch(e);
      if (typeof r["next"] === "number") this.after = r["next"];
    }
  }

  dispatch(e: Record<string, unknown>): void {
    const t = e["type"];
    if (t === "event" && typeof e["src"] === "string" && e["payload"] !== null && typeof e["payload"] === "object") {
      this.kernel.emit(e["src"], e["payload"] as Payload);
    } else if (t === "state_event" && e["payload"] !== null && typeof e["payload"] === "object") {
      this.toState({ ...(e["payload"] as Record<string, unknown>), type: "state_event" });
    } else if (t === "record_changed" && e["origin"] !== this.o.tab && typeof e["id"] === "string") {
      this.applyRecord(e["id"], e["removed"] === true ? null : (e["record"] as Record<string, unknown> | undefined) ?? null);
    }
  }

  /** Another tab's accepted page write, applied to this tab's tree. */
  applyRecord(id: string, rec: Record<string, unknown> | null): void {
    if (rec === null) {
      this.applied.set(id, "removed");
      if (this.kernel.get(id) !== undefined) this.kernel.remove(id);
      return;
    }
    this.applied.set(id, canon(rec));
    const { id: _i, handler_module: hm, parent_id: pid, ...meta } = rec;
    const a = this.kernel.get(id);
    if (a !== undefined) {
      this.kernel.updateMeta(id, meta as Payload);
      return;
    }
    const parentId = typeof pid === "string" && pid !== this.o.loader ? pid : null;
    if (parentId !== null && this.kernel.get(parentId) === undefined) return; // its parent is not shown here
    this.kernel.register(
      new Agent({ id, parentId, handlerModule: typeof hm === "string" ? hm : null, meta: meta as Payload }),
    );
  }

  close(): void {
    this.running = false;
    this.inner.close();
  }
}

/** The verbs a user's `can_call` holds on PAGE agent `id`, whose ancestors
 *  (nearest first) are `up`: its own entry, an ancestor's, `<guid>|*`, each
 *  also under the any-tree guid `*` — the host gateway's rule, the same. */
export function pageVerbs(cc: Record<string, unknown>, guid: string, id: string, up: readonly string[]): string[] {
  const out: string[] = [];
  const verbs = (v: unknown): string[] =>
    typeof v === "string" ? [v] : Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
  for (const g of [guid, "*"]) for (const k of [id, ...up, "*"]) out.push(...verbs(cc[`${g}|${k}`]));
  return out;
}

/** Writing a page record takes a verb other than `reflect`. */
export function writesPage(v: readonly string[]): boolean {
  return v.some((x) => x !== "reflect");
}
