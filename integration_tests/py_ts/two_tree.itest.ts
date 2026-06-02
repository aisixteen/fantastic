// Cross-language E2E — the REAL two-runtime round-trip: a live python `fantastic`
// daemon (host) + a real TS ProxyLoader over a real WsBridge. A frontend kernel
// persists a member through `web_loader` onto the host's disk; a SECOND frontend
// kernel rehydrates it over the wire. Proves the genuine cross-language link the
// in-process tests cover only in halves (python `test_two_tree.py` = the host
// disk side; ts `two_tree.test.ts` = the frontend re-root side).
// Run: npm run test:integration   (skips cleanly if the python env is absent).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { Kernel } from "../../ts/src/kernel/kernel.ts";
import { Agent } from "../../ts/src/kernel/agent.ts";
import { WsBridge } from "../../ts/src/transport/bridge.ts";
import { ProxyLoader } from "../../ts/src/bundles/loader/proxy_loader.ts";
import { registerHtmlAgent } from "../../ts/src/bundles/html_agent/html_agent.ts";
import { bootHost, teardownHost } from "./_host.ts";
import type { Host } from "./_host.ts";

let host: Host | null = null;
let skipReason = "";

before(async () => {
  try {
    // own port (bridge.itest.ts uses 8911) + a web_loader for the frontend store
    host = await bootHost(8913, { webLoader: true });
  } catch (e) {
    skipReason = `host unavailable: ${(e as Error).message}`;
  }
});

after(() => {
  if (host !== null) teardownHost(host);
});

interface Frontend {
  kernel: Kernel;
  loader: ProxyLoader;
  bridge: WsBridge;
}

/** A browser-side kernel: the view bundles registered + a real WsBridge dialing
 *  the host, and the ONE auto-added ProxyLoader proxying to `web_loader`. */
function frontend(h: Host): Frontend {
  const kernel = new Kernel();
  kernel.registerBundle("canvas.ts", () => null);
  registerHtmlAgent(kernel);
  const bridge = new WsBridge(kernel, {
    origin: h.origin,
    controlEndpoint: "web_loader",
  });
  const loader = new ProxyLoader(kernel, "web_loader", { debounceMs: 100000 });
  return { kernel, loader, bridge };
}

async function waitForFile(path: string, ms: number): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < ms) {
    if (existsSync(path)) return true;
    await new Promise((r) => setTimeout(r, 50));
  }
  return existsSync(path);
}

test("E2E: a frontend persists over a REAL WS to web_loader + rehydrates", async (t) => {
  if (host === null) return t.skip(skipReason);
  const h = host;

  // ── frontend 1: hydrate the (empty) namespace — this learns the host
  //    loader's REAL id from the anchor — then seed a canvas + persist a panel ──
  const fe1 = frontend(h);
  const recs0 = await fe1.loader.loadTree();
  if (recs0.length === 0) {
    // fresh namespace — seed the canvas root (mirrors main.ts's bootstrap)
    fe1.kernel.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  } else {
    fe1.kernel.load(recs0);
  }
  fe1.kernel.register(
    new Agent({
      id: "panel",
      parentId: fe1.kernel.rootId ?? "canvas",
      handlerModule: "html_agent.ts",
      meta: { html: "<p>e2e</p>" },
    }),
  );
  fe1.loader.start();
  await fe1.loader.flush();

  // ── the record crossed the wire + landed on the HOST's disk under web/ ──
  const panelJson = join(
    h.tmp,
    ".fantastic",
    "web",
    "agents",
    "canvas",
    "agents",
    "panel",
    "agent.json",
  );
  assert.ok(await waitForFile(panelJson, 3000), `expected ${panelJson} on host disk`);

  // ── frontend 2: a FRESH kernel rehydrates the SAME tree over the wire ──
  const fe2 = frontend(h);
  fe2.kernel.load(await fe2.loader.loadTree());
  assert.equal(fe2.kernel.rootId, "canvas");
  assert.ok(fe2.kernel.get("canvas")?.children.has("panel"), "panel rehydrated");
  assert.equal(fe2.kernel.get("panel")?.handlerModule, "html_agent.ts");
  assert.equal(fe2.kernel.get("panel")?.meta["html"], "<p>e2e</p>"); // content survived

  await fe1.loader.stop(); // clears the debounce timer (else the process hangs)
  fe1.bridge.close();
  fe2.bridge.close();
});
