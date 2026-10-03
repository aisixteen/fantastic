import type { Kernel } from "../kernel/kernel.ts";
import type { Json, Payload } from "../kernel/json.ts";
import type { Item } from "../kernel/outcome.ts";
import type { Mark } from "../kernel/secret.ts";

// The link a DOM view-agent uses to talk to the HOST kernel over the bridge.
// A thin, typed facade over Kernel — mirrors the old transport.js
// call/emit/watch/on surface so porting view logic is mechanical:
//   t.call(target, p)  -> host.call(target, p)
//   t.emit(target, p)  -> host.emit(target, p)
//   t.watch(src); t.on(type, fn) -> host.on(src, type, fn)
// (Note: the bridge keys event streams by their source socket, so `on` takes
// the src explicitly rather than dispatching globally by type.)
//
// TODO(rights): every call here goes out as the OPERATOR — `kernel.send` /
// `kernel.callHost` with no sender, although `selfId` is held. The page gate
// never checks an operator send, and the host sees it as the socket LEG
// (`wws`), which reaches all of its own children — so a view built on Host can
// reach ANY backend the leg reaches, not just its own. The fix (planned with
// the canvas move to the CLI project): send AS `selfId` (the view's own id as
// sender), and have the canvas create each view with an explicit
// `can_call: {<its backend_id>: [the verbs it fronts]}` — then the ts egress
// gate refuses view A → backend B on the page, before anything reaches the
// host. No kernel change needed; the gate already enforces agent senders.
export class Host {
  private readonly kernel: Kernel;
  private readonly selfId: string;

  constructor(kernel: Kernel, selfId: string) {
    this.kernel = kernel;
    this.selfId = selfId;
  }

  /** Verb call to a host agent; resolves the reply.
   *
   *  `send` is array-only, so a single call is a one-element batch whose one
   *  outcome we unwrap here. The OUTCOME is not discarded lightly: a refusal
   *  arrives in `result` like any other reply, which is exactly what the
   *  iframe/host callers already branch on.
   *
   *  `secret` — marks, each a dotted string (`payload.<field…>` /
   *  `result[.<field…>]`) or an array of steps (`["payload", "a.b"]`, a key
   *  holding a dot) — is the item's `secret`: this kernel's records of the call
   *  show `<secret>`, and a host-bound call carries the marks on its frame so the
   *  host masks too. */
  async call(target: string, payload: Payload, secret?: Mark[]): Promise<Json> {
    const item: Item = { target, payload };
    if (secret !== undefined) item.secret = secret;
    return (await this.kernel.send([item]))[0]!.result;
  }

  /** Call the HOST ROOT (or any host id) over the bridge, bypassing local
   *  resolution — pass `"kernel"` and the host resolves it to its own root
   *  (`kernel_state`/`core`). Used to read the host bundle catalog + create host
   *  peers without naming a host-specific id. `secret` rides the call frame. */
  callHost(target: string, payload: Payload, secret?: Mark[]): Promise<Json> {
    return this.kernel.callHost(target, payload, secret);
  }

  /** Fire-and-forget emit to a host agent's inbox (e.g. reload_html). */
  emit(target: string, payload: Payload): void {
    this.kernel.emitRemote(target, payload);
  }

  /** Verb call carrying a binary blob (e.g. paste_image Uint8Array); `secret`
   *  rides the call frame. */
  callBinary(target: string, payload: Record<string, unknown>, secret?: Mark[]): Promise<Json> {
    return this.kernel.forwardBinary(target, payload, secret);
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
