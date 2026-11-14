import { test } from "node:test";
import assert from "node:assert/strict";
import { sidebarGrant } from "../src/bundles/canvas/sidebar_mount.ts";
import { hearSidebar, pageStore } from "../../page/view_relay.js";
import type { SidebarEars } from "../../page/view_relay.js";
import { panelRelay } from "../src/bundles/panel_relay.ts";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";

const view = {
  id: "sidebar",
  view: {
    kind: "html",
    bytes: "<b>x</b>",
    calls: {
      kernel: ["reflect", "list_agents"],
      user: ["inquiries"],
      files: ["list", "watch", "watch_path", "unwatch_path", "open_view"],
      "*": ["status"],
      sidebar: ["watch"],
    },
  },
};

test("the sidebar cell is granted exactly the view's calls, `*` written out per host agent", () => {
  assert.deepEqual(sidebarGrant(view, ["chat1", "files"]), {
    kernel: ["reflect", "list_agents"],
    user: ["inquiries"],
    files: ["list", "watch", "watch_path", "unwatch_path", "open_view", "status"],
    sidebar: ["watch"],
    chat1: ["status"],
  });
  assert.equal(sidebarGrant(undefined, []), null, "no sidebar agent: no grant");
  assert.equal(sidebarGrant({ id: "sidebar" }, []), null, "no view: no grant");
});

test("a host agent's view says kernel for the HOST's root; any other panel keeps the page's", async () => {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  const hosts: string[] = [];
  (k as unknown as { callHost: (t: string, p: Payload) => Promise<unknown> }).callHost = async (t) => (hosts.push(t), { from: "host" });
  k.register(new Agent({ id: "cell", parentId: "canvas", meta: { backend_id: "sidebar", can_call: { kernel: ["reflect"] } } }));
  k.register(new Agent({ id: "plain", parentId: "canvas", meta: { can_call: { kernel: ["reflect"] } } }));
  const run = (id: string): unknown => {
    const prev = k.currentOrigin;
    (k as unknown as { currentOrigin: string | null }).currentOrigin = id;
    try {
      return panelRelay(k, { type: "panel_send", target: "kernel", payload: { type: "reflect" } });
    } finally {
      (k as unknown as { currentOrigin: string | null }).currentOrigin = prev;
    }
  };
  assert.deepEqual(await run("cell"), { from: "host" });
  assert.deepEqual(hosts, ["kernel"]);
  await run("plain");
  assert.deepEqual(hosts, ["kernel"], "a page-content panel still reaches the page's own kernel");
});

function ears(): { calls: unknown[][]; e: SidebarEars } {
  const calls: unknown[][] = [];
  return {
    calls,
    e: {
      select: (id) => calls.push(["select", id]),
      selectPath: (p) => calls.push(["selectPath", p]),
      open: (p, dir) => calls.push(["open", p, dir]),
      hide: () => calls.push(["hide"]),
      ready: () => calls.push(["ready"]),
      keep: (s) => calls.push(["keep", s]),
    },
  };
}

test("each thing the view posts is heard once; anything else is ignored", () => {
  const { calls, e } = ears();
  for (const m of [
    { type: "select", id: "chat1" },
    { type: "select_file", path: "src/a.rs" },
    { type: "open_file", path: "src/a.rs" },
    { type: "open_dir", path: "src" },
    { type: "hide_tree" },
    { type: "ready" },
    { type: "state", state: { open: { src: true } } },
  ]) hearSidebar(m, e);
  assert.deepEqual(calls, [
    ["select", "chat1"],
    ["selectPath", "src/a.rs"],
    ["open", "src/a.rs", false],
    ["open", "src", true],
    ["hide"],
    ["ready"],
    ["keep", { open: { src: true } }],
  ]);
});

test("a message with no id or path, the kernel strip's, an echo of the page's own restore: nothing happens", () => {
  const { calls, e } = ears();
  for (const m of [{ type: "select" }, { type: "select", id: "" }, { type: "select_file" }, { type: "open_file", path: 3 }, { type: "select_kernel", guid: "abc" }, { type: "restore", state: {} }, {}]) hearSidebar(m, e);
  assert.deepEqual(calls, []);
});

test("the page keeps the view's open state under its key, and a browser with no storage keeps nothing", () => {
  const mem = new Map<string, string>();
  const store = pageStore("sidebar:canvas", () => ({ getItem: (k) => mem.get(k) ?? null, setItem: (k, v) => void mem.set(k, v) }));
  assert.equal(store.load(), null, "nothing kept yet");
  store.save({ filesOn: true, open: { src: true } });
  assert.deepEqual([...mem.keys()], ["fantastic.sidebar:canvas"]);
  assert.deepEqual(store.load(), { filesOn: true, open: { src: true } });
  mem.set("fantastic.sidebar:canvas", "{not json");
  assert.equal(store.load(), null, "a damaged value is no state");
  const none = pageStore("k", () => {
    throw new Error("storage refused");
  });
  none.save({ a: 1 });
  assert.equal(none.load(), null);
});
