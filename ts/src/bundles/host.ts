import type { Kernel } from "../kernel/kernel.ts";
import type { Json, Payload } from "../kernel/json.ts";

// The link a DOM view-agent uses to talk to the HOST kernel over the bridge.
// A thin, typed facade over Kernel — mirrors the old transport.js
// call/emit/watch/on surface so porting view logic is mechanical:
//   t.call(target, p)  -> host.call(target, p)
//   t.emit(target, p)  -> host.emit(target, p)
//   t.watch(src); t.on(type, fn) -> host.on(src, type, fn)
// (Note: the bridge keys event streams by their source socket, so `on` takes
// the src explicitly rather than dispatching globally by type.)
export class Host {
  private readonly kernel: Kernel;
  private readonly selfId: string;

  constructor(kernel: Kernel, selfId: string) {
    this.kernel = kernel;
    this.selfId = selfId;
  }

  /** Verb call to a host agent; resolves the reply. */
  call(target: string, payload: Payload): Promise<Json> {
    return this.kernel.send(target, payload);
  }

  /** Call the HOST ROOT (or any host id) over the bridge, bypassing local
   *  resolution — pass `"kernel"` and the host resolves it to its own root
   *  (`kernel_state`/`core`). Used to read the host bundle catalog + create host
   *  peers without naming a host-specific id. */
  callHost(target: string, payload: Payload): Promise<Json> {
    return this.kernel.callHost(target, payload);
  }

  /** Fire-and-forget emit to a host agent's inbox (e.g. reload_html). */
  emit(target: string, payload: Payload): void {
    this.kernel.emitRemote(target, payload);
  }

  /** Verb call carrying a binary blob (e.g. paste_image Uint8Array). */
  callBinary(target: string, payload: Record<string, unknown>): Promise<Json> {
    return this.kernel.forwardBinary(target, payload);
  }

  /** Watch `src` and call `handler` for each `type` event on its inbox.
   *  Returns an unsubscribe fn (drops the listener; leaves the watch open). */
  on(src: string, type: string, handler: (payload: Payload) => void): () => void {
    this.kernel.watch(src, this.selfId);
    return this.kernel.onInbox(src, (payload) => {
      if (payload["type"] === type) handler(payload);
    });
  }

  /** Stop watching a src entirely (closes its socket if no one else watches). */
  unwatch(src: string): void {
    this.kernel.unwatch(src, this.selfId);
  }

  /** connect/disconnect notifications for the host backend's socket. */
  onLifecycle(
    src: string,
    handler: (state: "connected" | "disconnected") => void,
  ): () => void {
    return this.kernel.onLifecycle(src, handler);
  }

  /** Subscribe to the host kernel state stream (telemetry). */
  subscribeState(handler: (frame: Record<string, unknown>) => void): () => void {
    return this.kernel.subscribeState(handler);
  }
}
