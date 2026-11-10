import { test } from "node:test";
import assert from "node:assert/strict";
import { sidebarGrant } from "../src/bundles/canvas/sidebar_mount.ts";
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
      files: ["list", "watch_path", "unwatch_path", "open_view"],
      "*": ["status"],
      sidebar: ["watch"],
    },
  },
};

test("the sidebar cell is granted exactly the view's calls, `*` written out per host agent", () => {
  assert.deepEqual(sidebarGrant(view, ["chat1", "files"]), {
    kernel: ["reflect", "list_agents"],
    user: ["inquiries"],
    files: ["list", "watch_path", "unwatch_path", "open_view", "status"],
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
