// kernel_bridge integration — the WsBridge against a LIVE python host.
// Boots web + nested web_ws, then round-trips real frames over a real socket.
// Run: npm run test:integration   (skips cleanly if the python env is absent).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../../ts/src/kernel/kernel.ts";
import { Agent } from "../../ts/src/kernel/agent.ts";
import { WsBridge } from "../../ts/src/transport/bridge.ts";
import { bootHost, teardownHost } from "./_host.ts";
import type { Host } from "./_host.ts";

let host: Host | null = null;
let skipReason = "";

before(async () => {
  try {
    host = await bootHost(8911);
  } catch (e) {
    skipReason = `host unavailable: ${(e as Error).message}`;
  }
});

after(() => {
  if (host !== null) teardownHost(host);
});

function freshBridge(h: Host): { kernel: Kernel; bridge: WsBridge } {
  const kernel = new Kernel();
  kernel.setRoot(new Agent({ id: "fs_loader" }));
  const bridge = new WsBridge(kernel, {
    origin: h.origin,
    controlEndpoint: "fs_loader",
  });
  return { kernel, bridge };
}

test("reflect kernel over the live wire returns the canonical shape", async (t) => {
  if (host === null) return t.skip(skipReason);
  const { bridge } = freshBridge(host);
  try {
    const r = (await bridge.forward("kernel", {
      type: "reflect",
      tree: "ids",
      bundles: "ids",
    })) as Record<string, unknown>;
    assert.equal(r["id"], "fs_loader");
    assert.match(String(r["sentence"]), /^Fantastic kernel/);
    const tree = r["tree"] as string[];
    assert.ok(Array.isArray(tree) && tree.includes(host.webId), "tree includes web id");
    const bundles = r["bundles"] as string[];
    assert.ok(Array.isArray(bundles) && bundles.includes("web_ws"), "bundles catalog present");
  } finally {
    bridge.close();
  }
});

test("forward a call to fs_loader (list_agents), and reflect a non-root leaf", async (t) => {
  if (host === null) return t.skip(skipReason);
  const { bridge } = freshBridge(host);
  try {
    const listed = (await bridge.forward("fs_loader", { type: "list_agents" })) as {
      agents: Array<{ id: string }>;
    };
    const ids = listed.agents.map((a) => a.id);
    assert.ok(ids.includes(host.webId), "list_agents includes the web agent");

    // forwarding to a non-root target (the web agent) reaches its OWN handler:
    // python bundle agents answer reflect themselves, so web returns its
    // bundle-specific shape (id + verbs + port), not the bare identity block.
    const leaf = (await bridge.forward(host.webId, {
      type: "reflect",
      tree: "none",
    })) as Record<string, unknown>;
    assert.equal(leaf["id"], host.webId, "reflect routed to the web agent");
    assert.ok(leaf["verbs"], "web's own reflect carries its verb catalog");
  } finally {
    bridge.close();
  }
});

test("watchRemote streams a host agent's inbox events to a local watcher", async (t) => {
  if (host === null) return t.skip(skipReason);
  const { kernel, bridge } = freshBridge(host);
  try {
    // a local view-agent that watches the web agent's inbox
    kernel.register(new Agent({ id: "viewer", parentId: "fs_loader" }));
    const events: Array<Record<string, unknown>> = [];
    kernel.onInbox("viewer", (p) => events.push(p));
    kernel.watch(host.webId, "viewer"); // remote → bridge.watchRemote(web)

    // create a child agent under the web agent → web emits `agent_created`
    // on its own inbox (see _verb_create_agent), which our socket mirrors.
    await bridge.forward(host.webId, {
      type: "create_agent",
      handler_module: "web_rest.tools",
    });

    const start = Date.now();
    while (events.length === 0 && Date.now() - start < 5000) {
      await new Promise((r) => setTimeout(r, 50));
    }
    assert.ok(
      events.some((e) => e["type"] === "agent_created"),
      `expected an agent_created event, got ${JSON.stringify(events)}`,
    );
  } finally {
    bridge.close();
  }
});
