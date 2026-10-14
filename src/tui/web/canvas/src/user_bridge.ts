// THE PAGE THROUGH `user` (owner, 2026-10-03, single user): every host CALL
// this tab makes goes to the host's `user` (`user.tools`), and `user` makes it:
//
//   · a call to host agent T  → `user.forward {target: T, payload}`;
//   · a call to `user` itself  → its own payload;
//   · emits, watches, the state stream, socket lifecycle → the leg's own
//                               frames (the lib bridge, unchanged).

import type { Bridge, Json, Kernel, Payload, WsBridge } from "./lib.ts";

/** The host's one user: every host call goes through it. */
export const GATEWAY = "user";
export class UserBridge implements Bridge {
  private readonly inner: WsBridge;

  constructor(kernel: Kernel, inner: WsBridge) {
    this.inner = inner;
    kernel.bridge = this;
  }

  private wrap(target: string, payload: Payload | Record<string, unknown>): Record<string, unknown> {
    if (target === GATEWAY) return { ...payload };
    return { type: "forward", target, payload };
  }

  forward(target: string, payload: Payload): Promise<Json> {
    return this.inner.forward(GATEWAY, this.wrap(target, payload) as Payload);
  }

  forwardBinary(target: string, payload: Record<string, unknown>): Promise<Json> {
    return this.inner.forwardBinary(GATEWAY, this.wrap(target, payload));
  }

  /** A host agent's inbox, fire-and-forget: a real emit, the leg's own frame
   *  (gated on the host as the leg's `emit`) — never a call. */
  emitRemote(target: string, payload: Payload): void {
    this.inner.emitRemote(target, payload);
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
