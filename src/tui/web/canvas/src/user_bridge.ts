// THE PAGE THROUGH `user` (owner, 2026-10-03, single user): every host CALL
// this tab makes goes to the host's `user` (`user.tools`) with the tab's token,
// if one is set, and `user` makes it:
//
//   · a call to host agent T  → `user.forward {target: T, payload, token}`;
//   · a call to `user` itself  → its own payload, with the token;
//   · watches, the state stream, socket lifecycle → the leg's own frames (the
//                               lib bridge, unchanged).
//
// The token lives in browser storage only (never the URL), read once per load.

import type { Bridge, Json, Kernel, Payload, WsBridge } from "./lib.ts";

/** The host's one user: every host call goes through it. */
export const GATEWAY = "user";
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

export interface UserBridgeOptions {
  /** This tab's token (read once at load). */
  token: () => string | null;
}

export class UserBridge implements Bridge {
  private readonly inner: WsBridge;
  private readonly o: UserBridgeOptions;

  constructor(kernel: Kernel, inner: WsBridge, o: UserBridgeOptions) {
    this.inner = inner;
    this.o = o;
    kernel.bridge = this;
  }

  private wrap(target: string, payload: Payload | Record<string, unknown>): Record<string, unknown> {
    const token = this.o.token();
    if (target === GATEWAY) return { ...payload, ...(token !== null ? { token } : {}) };
    const w: Record<string, unknown> = { type: "forward", target, payload };
    if (token !== null) w["token"] = token;
    return w;
  }

  forward(target: string, payload: Payload): Promise<Json> {
    return this.inner.forward(GATEWAY, this.wrap(target, payload) as Payload);
  }

  forwardBinary(target: string, payload: Record<string, unknown>): Promise<Json> {
    return this.inner.forwardBinary(GATEWAY, this.wrap(target, payload));
  }

  /** A host agent's inbox, fire-and-forget: `user` sends it as a call. */
  emitRemote(target: string, payload: Payload): void {
    void this.forward(target, { ...payload });
  }

  // THE LEG'S OWN FRAMES: watches, the state stream, a socket's lifecycle.
  watchRemote(src: string): void {
    this.inner.watchRemote(src);
  }

  unwatchRemote(src: string): void {
    this.inner.unwatchRemote(src);
  }

  onLifecycle(endpoint: string, fn: (state: "connected" | "disconnected") => void): () => void {
    return this.inner.onLifecycle(endpoint, fn);
  }

  subscribeState(handler: (frame: Record<string, unknown>) => void): () => void {
    return this.inner.subscribeState(handler);
  }

  close(): void {
    this.inner.close();
  }
}
