import { test } from "node:test";
import assert from "node:assert/strict";
import { clocksReadNothing, file, ownCopiesReadNothing, run } from "./view_harness.ts";
import type { Obj } from "./view_harness.ts";

test("the sidebar watches every agent it shows and re-reads only what an event names", async () => {
  const tree = { id: "kernel", children: [{ id: "chat1", handler_module: "ai_chat.tools", children: [] }, { id: "files", children: [] }, { id: "web", children: [] }] };
  const h = run(file("src/tui/src/kern/sidebar_view.html"), { agent: "sidebar", cell: "view_sidebar" }, (target, p) => {
    if (target === "kernel" && p["handler_modules"]) return { tree: ["chat1"] };
    if (target === "kernel") return { id: "kernel", guid: "g1", peers: [], tree };
    if (target === "files") return { root: "/p/proj" };
    if (target === "user") return { inquiries: [] };
    return { current: null };
  });
  await h.settle();
  for (const id of ["kernel", "chat1", "files", "web"]) assert.ok(h.watches.has(id), `the tree watches ${id}`);
  assert.deepEqual(h.intervals.map((t) => t.ms), [60000], "one clock, for the ages");
  await clocksReadNothing(h);

  const reads = async (src: string, ev: Obj): Promise<string[]> => {
    const mark = h.sends.length;
    await h.publish(src, ev);
    return h.since(mark).map((s) => `${s.target} ${String(s.p["type"])}`);
  };
  assert.deepEqual(await reads("chat1", { type: "status", phase: "thinking" }), ["chat1 status"], "a chat's call moved: its status");
  assert.deepEqual(await reads("chat1", { type: "inquiry_opened", re: 4, asker: "chat1", holder: "user" }), ["user inquiries"]);
  assert.deepEqual(await reads("chat1", { type: "inquiry_opened", re: 5, asker: "chat1", holder: "mail" }), [], "another holder's questions are not the person's list");
  assert.deepEqual((await reads("kernel", { type: "agent_created", id: "x" })).filter((s) => s.startsWith("kernel")), ["kernel reflect", "kernel reflect"], "the tree is read again");
  assert.deepEqual((await reads("web", { type: "peer_connected", guid: "g2" })).filter((s) => s.startsWith("kernel")), ["kernel reflect", "kernel reflect"], "a kernel connected: the strip is read again");
  assert.deepEqual(await reads("web", { type: "status", phase: "running" }), [], "a status of an agent that is not a chat reads nothing");
  assert.deepEqual(await reads("chat1", { type: "send", text: "hi" }), [], "a call to a chat is not a change of it");
});

test("the sidebar never reads again on a copy of a call — its own reads, another client's status read", async () => {
  const tree = { id: "kernel", children: [{ id: "chat1", handler_module: "ai_chat.tools", children: [] }, { id: "user", children: [] }] };
  const h = run(file("src/tui/src/kern/sidebar_view.html"), { agent: "sidebar", cell: "view_sidebar" }, (target, p) => {
    if (target === "kernel" && p["handler_modules"]) return { tree: ["chat1"] };
    if (target === "kernel") return { id: "kernel", guid: "g1", peers: [], tree };
    if (target === "files") return { root: "/p/proj" };
    if (target === "user") return { inquiries: [] };
    return { current: null };
  });
  await h.settle();
  const booted = h.sends.map((s) => `${s.target} ${String(s.p["type"])}`);
  assert.ok(booted.includes("chat1 status") && booted.includes("kernel reflect"), `${booted}`);
  // the watch delivered the copies of every boot read already: the view is quiet
  await ownCopiesReadNothing(h);
  const reads = async (src: string, ev: Obj): Promise<string[]> => {
    const mark = h.sends.length;
    await h.publish(src, ev);
    return h.since(mark).map((s) => `${s.target} ${String(s.p["type"])}`);
  };
  assert.deepEqual(await reads("chat1", { type: "status" }), [], "a status read (no phase) is a call, not a change");
  assert.deepEqual(await reads("chat1", { type: "status", client_id: "other-view" }), [], "another client's status read");
  assert.deepEqual(await reads("user", { type: "inquiries" }), [], "an inquiries read");
  assert.deepEqual(await reads("kernel", { type: "reflect", detail: "full" }), [], "a reflect");
  // a real event still reads, once, and its own read's copy does not read again
  assert.deepEqual(await reads("chat1", { type: "status", phase: "thinking", source: "chat1" }), ["chat1 status"]);
});
