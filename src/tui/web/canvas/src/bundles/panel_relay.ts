import { str } from "../lib.ts";
import type { Json, Kernel, Payload } from "../lib.ts";

// A PANEL'S OWN CALLS (an html panel's sandboxed iframe, a GL source) go out AS
// ITS AGENT, never as the operator: the panel holds content the operator did
// not necessarily write, so its reach is its agent's `can_call`. The kernel has
// no API to send as anyone else — identity comes only from dispatch — so the
// page hands each one to the agent's own handler, whose sends ARE the agent's:
//
//   panel_send  {target, payload} → that call's result ("kernel" from a host agent's view is the host's root)
//   panel_emit  {target, payload} → an emit (a local agent's inbox, or a host
//                                   agent's over the bridge, gated by `emit` in
//                                   this agent's can_call); dropped if denied
//   panel_watch {src}             → null when allowed, else the refusal

/** The verbs, when `payload` is one of them; undefined otherwise. */
export function panelRelay(kernel: Kernel, payload: Payload): Json | Promise<Json> | undefined {
  const target = str(payload, "target");
  const inner = (payload["payload"] ?? {}) as Payload;
  switch (str(payload, "type")) {
    case "panel_send":
      // a HOST agent's view (its cell carries `backend_id`) says "kernel" for the HOST's root: the
      // page's own "kernel" is the canvas root, so the literal goes over the bridge (the cell's
      // `can_call` still judges it, `callHost`)
      if (target === "kernel" && kernel.currentOrigin !== null && typeof kernel.get(kernel.currentOrigin)?.meta["backend_id"] === "string") {
        return kernel.callHost(target, inner);
      }
      return kernel.send([{ target, payload: inner }]).then((outs) => outs[0]!.result);
    case "panel_emit": {
      if (kernel.get(target) !== undefined) {
        kernel.emit(target, inner);
        return null;
      }
      // a HOST agent: gated as `callHost` is — this agent's `can_call` must
      // grant `emit` on the target; then a real emit, never a call
      const denied = kernel.currentOrigin === null ? null : kernel.egressGate(kernel.currentOrigin, "emit", target);
      if (denied === null) kernel.emitRemote(target, inner);
      return null;
    }
    case "panel_watch":
      return kernel.watch(str(payload, "src"), str(payload, "watcher")) as unknown as Json;
    default:
      return undefined;
  }
}
