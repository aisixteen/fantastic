import { test } from "node:test";
import assert from "node:assert/strict";
import { clocksReadNothing, file, ownCopiesReadNothing, run } from "./view_harness.ts";
import type { Harness, Obj } from "./view_harness.ts";

// THE SIDEBAR VIEW WATCHES ONE AGENT, its own: the sidebar agent follows the kernel and
// publishes what the tree shows changed (`tree_changed`, `status_changed {agent, phase}`,
// `inquiries_changed {open}`, `files_changed {path, change}`). Its other calls are reads.

const tree = { id: "kernel", children: [{ id: "chat1", handler_module: "ai_chat.tools", children: [] }, { id: "files", children: [] }, { id: "web", children: [] }, { id: "user", children: [] }] };

function sidebar(): Harness {
  return run(file("src/tui/src/kern/sidebar_view.html"), { agent: "sidebar", cell: "view_sidebar" }, (target, p) => {
    if (target === "kernel" && p["handler_modules"]) return { tree: ["chat1"] };
    if (target === "kernel") return { id: "kernel", guid: "g1", peers: [], tree };
    if (target === "files" && p["type"] === "list") return { files: [{ name: "src", path: "src", type: "dir" }] };
    if (target === "files") return {};
    if (target === "user") return { inquiries: [] };
    if (target === "sidebar") return { id: "sidebar", phases: { chat1: "thinking" }, open: 0, files_root: "/p/proj" };
    return { error: `unexpected read of ${target}` };
  });
}

const kinds = (h: Harness, mark: number): string[] => h.since(mark).map((s) => `${s.target} ${String(s.p["type"])}`);

test("the sidebar view watches its own agent alone, and reads the chats' phases from it", async () => {
  const h = sidebar();
  await h.settle();
  assert.deepEqual([...h.watches.keys()], ["sidebar"], "one agent watched: its own");
  const booted = kinds(h, 0);
  assert.ok(booted.includes("sidebar reflect") && booted.includes("kernel reflect") && booted.includes("user inquiries"), `${booted}`);
  assert.ok(!booted.some((s) => s.endsWith(" status")), `no chat is read: ${booted}`);
  assert.ok(!booted.includes("files reflect"), `the project's root comes from its own agent: ${booted}`);
  assert.deepEqual(h.intervals.map((t) => t.ms), [60000], "one clock, for the ages");
  await clocksReadNothing(h);
});

test("each event of its agent reads only what it names; a status carries its phase and reads nothing", async () => {
  const h = sidebar();
  await h.settle();
  const reads = async (ev: Obj): Promise<string[]> => {
    const mark = h.sends.length;
    await h.publish("sidebar", ev);
    return kinds(h, mark);
  };
  assert.deepEqual(await reads({ type: "status_changed", agent: "chat1", phase: "streaming" }), [], "the phase is in the event");
  assert.deepEqual(await reads({ type: "inquiries_changed", open: 1 }), ["user inquiries"]);
  assert.deepEqual((await reads({ type: "tree_changed" })).filter((s) => s.startsWith("kernel")), ["kernel reflect", "kernel reflect"], "the tree is read again");
  assert.deepEqual(await reads({ type: "files_changed", path: "src/a.rs", change: "added" }), [], "the view lists no files: nothing to read");
});

test("with the files shown, an entry that came or went reads its folder; a write in place is never published", async () => {
  const h = sidebar();
  await h.settle();
  // the host gives back what it kept (files shown): the view lists the root and asks the files agent to watch it
  await h.message({ type: "restore", state: { folded: {}, open: {}, filesOn: true, toolsOpen: false } });
  const listed = kinds(h, 0);
  assert.ok(listed.includes("files watch_path") && listed.includes("files list"), `${listed}`);
  const reads = async (ev: Obj): Promise<string[]> => {
    const mark = h.sends.length;
    await h.publish("sidebar", ev);
    return kinds(h, mark);
  };
  assert.deepEqual(await reads({ type: "files_changed", path: "a.txt", change: "added" }), ["files list"]);
  assert.deepEqual(await reads({ type: "files_changed", path: "gone.txt", change: "removed" }), ["files list"]);
  assert.deepEqual(await reads({ type: "files_changed", path: "deep/x/y", change: "added" }), [], "a folder the view does not list");
  assert.deepEqual([...h.watches.keys()], ["sidebar"], "the files agent is not watched");
});

test("the sidebar view never reads again on a copy of a call — its own reads, another client's", async () => {
  const h = sidebar();
  await h.settle();
  await ownCopiesReadNothing(h);
  const reads = async (ev: Obj): Promise<string[]> => {
    const mark = h.sends.length;
    await h.publish("sidebar", ev);
    return kinds(h, mark);
  };
  for (const copy of [{ type: "reflect" }, { type: "reflect", detail: "full" }, { type: "watch" }, { type: "boot" }, { type: "status" }, { type: "inquiries" }])
    assert.deepEqual(await reads(copy), [], JSON.stringify(copy));
  // a real event still reads, once, and the copy of its own read does not read again
  assert.deepEqual(await reads({ type: "inquiries_changed", open: 2 }), ["user inquiries"]);
});
