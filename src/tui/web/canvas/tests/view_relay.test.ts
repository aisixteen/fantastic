import { test } from "node:test";
import assert from "node:assert/strict";
import { mountView, connectorSrc, callAllowed, hearSidebar } from "../../page/view_relay.js";
import { CONNECTOR_SRC } from "../src/bundles/html_agent/connector.ts";
import { MEASURE_SRC } from "../src/bundles/canvas/fit.ts";

// A tiny window and iframe: messages the view "posts" are dispatched by hand; what the relay posts back is kept.
function rig(extra: Record<string, unknown> = {}) {
  const listeners: ((e: unknown) => void)[] = [];
  const win = {
    addEventListener: (_t: string, f: (e: unknown) => void) => listeners.push(f),
    removeEventListener: (_t: string, f: (e: unknown) => void) => listeners.splice(listeners.indexOf(f), 1),
  };
  const sent: Record<string, unknown>[] = [];
  const contentWindow = { postMessage: (m: Record<string, unknown>) => sent.push(m) };
  const attrs = new Map<string, string>();
  const iframe = {
    srcdoc: "",
    contentWindow,
    removed: false,
    setAttribute: (k: string, v: string) => attrs.set(k, v),
    remove() {
      this.removed = true;
    },
  };
  const mount = { kids: [] as unknown[], appendChild(n: unknown) { this.kids.push(n); } };
  const doc = { createElement: () => iframe };
  const post = (m: Record<string, unknown>, source: unknown = contentWindow) => listeners.forEach((f) => f({ source, data: { __ft: true, ...m } }));
  const calls: unknown[][] = [];
  const base = {
    mount,
    doc,
    win,
    html: "<p>hi {{cell}}</p>",
    transport: {
      send: async (t: string, p: unknown) => {
        calls.push(["send", t, p]);
        return { ok: t };
      },
      watch: async (s: string, deliver: (p: unknown) => void) => {
        calls.push(["watch", s]);
        deliver({ first: 1 });
        return { off: () => calls.push(["off", s]) };
      },
      emit: async (t: string, p: unknown) => {
        calls.push(["emit", t, p]);
      },
    },
    ...extra,
  };
  return { view: mountView(base), iframe, mount, sent, attrs, post, listeners, calls };
}
const tick = () => new Promise((r) => setTimeout(r, 0));

test("the canvas connector is the module's connector plus the size measure", () => {
  assert.equal(CONNECTOR_SRC, connectorSrc(MEASURE_SRC));
  assert.match(connectorSrc(), /globalThis\.fantastic = \{/);
});

test("mounts a sandboxed iframe whose srcdoc carries the connector and the body", async () => {
  const r = rig({ cell: "view_x" });
  await tick();
  assert.equal(r.attrs.get("sandbox"), "allow-scripts");
  assert.equal(r.mount.kids.length, 1);
  assert.ok(r.iframe.srcdoc.includes("<p>hi view_x</p>"));
  assert.ok(r.iframe.srcdoc.includes(connectorSrc()));
});

test("a trusted view is not sandboxed", () => {
  const r = rig({ trusted: true });
  assert.equal(r.attrs.has("sandbox"), false);
});

test("send goes through the transport and the reply comes back by rid; foreign windows are ignored", async () => {
  const r = rig();
  r.post({ op: "send", rid: "7", target: "a", payload: { type: "go" } }, {});
  await tick();
  assert.equal(r.calls.length, 0);
  r.post({ op: "send", rid: "7", target: "a", payload: { type: "go" } });
  await tick();
  assert.deepEqual(r.calls, [["send", "a", { type: "go" }]]);
  assert.deepEqual(r.sent.at(-1), { __ft: true, op: "reply", rid: "7", data: { ok: "a" } });
});

test("calls gate a send and a watch before the transport", async () => {
  const r = rig({ calls: { files: ["list"], "*": ["reflect"] } });
  r.post({ op: "send", rid: "1", target: "files", payload: { type: "list" } });
  r.post({ op: "send", rid: "2", target: "files", payload: { type: "delete" } });
  r.post({ op: "send", rid: "3", target: "x", payload: { type: "reflect" } });
  r.post({ op: "watch", src: "files" });
  await tick();
  assert.deepEqual(r.calls.map((c) => c[0] + ":" + c[1]), ["send:files", "send:x"]);
  const second = r.sent.find((m) => m.rid === "2") as { data: { reason: string } };
  assert.equal(second.data.reason, "denied");
  assert.equal(r.sent.find((m) => m.op === "event")!.src, "files");
  assert.equal(callAllowed({ a: ["*"] }, "a", "x"), true);
});

test("watch delivers events, a second watch is not doubled, unwatch drops it", async () => {
  const r = rig();
  r.post({ op: "watch", src: "s" });
  r.post({ op: "watch", src: "s" });
  await tick();
  assert.equal(r.calls.filter((c) => c[0] === "watch").length, 1);
  assert.deepEqual(r.sent.find((m) => m.op === "event"), { __ft: true, op: "event", src: "s", payload: { first: 1 } });
  r.post({ op: "unwatch", src: "s" });
  assert.deepEqual(r.calls.at(-1), ["off", "s"]);
});

test("unwatched while the gate answers: what was subscribed is undone", async () => {
  const r = rig();
  r.post({ op: "watch", src: "s" });
  r.post({ op: "unwatch", src: "s" });
  await tick();
  assert.deepEqual(r.calls.at(-1), ["off", "s"]);
});

test("a denied watch reports the denial back and holds nothing", async () => {
  const r = rig();
  r.view.unmount();
  const q = rig({ transport: { send: async () => 0, watch: async () => ({ denied: { error: "no" } }) } });
  q.post({ op: "watch", src: "s" });
  await tick();
  assert.deepEqual(q.sent.at(-1), { __ft: true, op: "event", src: "s", payload: { error: "no" } });
  q.post({ op: "watch", src: "s" });
  await tick();
  assert.equal(q.sent.filter((m) => m.op === "event").length, 2);
});

test("emit to another agent goes to the transport; emit to the cell is the view speaking to its page", async () => {
  const heard: unknown[] = [];
  const r = rig({ cell: "view_c", onMessage: (m: unknown) => heard.push(m) });
  r.post({ op: "emit", target: "other", payload: { type: "x" } });
  r.post({ op: "emit", target: "view_c", payload: { type: "select", id: "a" } });
  await tick();
  assert.deepEqual(r.calls, [["emit", "other", { type: "x" }]]);
  assert.deepEqual(heard, [{ type: "select", id: "a" }]);
});

test("other posted messages (size, open_canvas) reach onMessage whole", () => {
  const heard: { op?: string }[] = [];
  const r = rig({ onMessage: (m: { op?: string }) => heard.push(m) });
  r.post({ op: "size", h: 40, mw: 10 });
  r.post({ op: "open_canvas", target: "c" });
  assert.deepEqual(heard.map((m) => m.op), ["size", "open_canvas"]);
});

test("kept state: ready restores what was kept, state keeps what the view emits", () => {
  let kept: unknown = { open: ["a"] };
  const r = rig({ cell: "view_c", keep: { load: () => kept, save: (s: unknown) => (kept = s) } });
  r.post({ op: "emit", target: "view_c", payload: { type: "ready" } });
  assert.deepEqual(r.sent.at(-1), { __ft: true, op: "inbox", payload: { type: "restore", state: { open: ["a"] } } });
  r.post({ op: "emit", target: "view_c", payload: { type: "state", state: { open: ["b"] } } });
  assert.deepEqual(kept, { open: ["b"] });
});

test("the cell's inbox goes to the view; reload_html renders again; unmount cleans up", async () => {
  let deliver: (p: unknown) => void = () => {};
  let off = 0;
  let body = "one";
  const r = rig({ html: () => body, inbox: (d: (p: unknown) => void) => { deliver = d; return () => off++; } });
  await tick();
  deliver({ type: "hello" });
  assert.deepEqual(r.sent.at(-1), { __ft: true, op: "inbox", payload: { type: "hello" } });
  body = "two";
  deliver({ type: "reload_html" });
  await tick();
  assert.ok(r.iframe.srcdoc.includes("two"));
  r.post({ op: "watch", src: "s" });
  await tick();
  r.view.unmount();
  assert.equal(off, 1);
  assert.equal(r.iframe.removed, true);
  assert.equal(r.listeners.length, 0);
  assert.deepEqual(r.calls.at(-1), ["off", "s"]);
});

test("a page that needs only some of what the sidebar view says leaves the other ears out", () => {
  const got: string[] = [];
  for (const m of [{ type: "select", id: "a" }, { type: "open_file", path: "p" }, { type: "hide_tree" }, { type: "ready" }, { type: "state", state: 1 }]) hearSidebar(m, { select: (id) => got.push(id) });
  assert.deepEqual(got, ["a"]);
});
