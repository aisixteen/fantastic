// The iframe-side CONNECTOR — injected into an html_agent's srcdoc as a plain
// <script>. It exposes a kernel-mirroring API (`fantastic.send/emit/watch/
// onMessage`) that talks ONLY to the embedding page via `postMessage`. The page's
// `html_view` relay bridges it to the JS kernel; the JS kernel — the SOLE owner of
// the host link (the kernel bridge) — routes to other FRONTEND agents locally or
// to HOST agents over the bridge, transparently. The iframe has NO host URL, NO
// WebSocket, NO same-origin access: postMessage to its parent is its only channel.
// The connector text is the base's ONE view relay (`web/page/view_relay.js`); the canvas adds its size measure.
// (This replaces the old `_bridge.js`, which bypassed the JS kernel by dialing the
// host directly — never again.)
//
//   const { job_id } = await fantastic.send("exec_bridge_x", { type:"start", command:"python3", args:["-c","print(1+1)"] });
//   fantastic.watch("exec_bridge_x", (ev) => { if (ev.job_id===job_id) render(ev); }); // live progress
//   fantastic.emit("panel2", { type:"value", value });                     // → another agent
//   const off = fantastic.watch("exec_bridge_x", (ev) => …);            // ← agent events
//   fantastic.onMessage((p) => …);                                         // ← messages sent to ME
//   fantastic.openCanvas("canvas_child_a");                              // → the page switches to that canvas

import { MEASURE_SRC } from "../canvas/fit.ts";
import { connectorSrc } from "../../../../page/view_relay.js";

export const CONNECTOR_SRC = connectorSrc(MEASURE_SRC);
