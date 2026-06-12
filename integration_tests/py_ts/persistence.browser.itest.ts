// Integration: save/load across a REAL daemon restart. The host kernel_state unit tests
// cover host-side persist→reboot→rehydrate; this proves the FULL stack — the frontend
// store (web_loader) + a RUNTIME-persisted record + the browser re-hydration — all
// survive killing and re-spawning the daemon process.
//
// p1 is seeded on disk; on load p1 persists a SECOND record (p2) to the host
// web_loader (a runtime write). We restart the daemon, reload the browser, and assert
// BOTH the seeded (p1) and the runtime-persisted (p2) panels come back from disk.
//
// Run: (cd ts && npm run build) then: cd integration_tests/py_ts &&
//      node --test --test-force-exit persistence.browser.itest.ts

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { join } from "node:path";
import { bootHost, teardownHost, restartHost, DIST_DIR, writeServedDist } from "./_host.ts";
import type { Host } from "./_host.ts";
import { Browser, chromeAvailable } from "./_chrome.ts";

const MOUNT_HTML = `<!doctype html><html><head><meta charset="utf-8">
<title>fantastic · persistence test</title>
<link rel="stylesheet" href="/ts_dist/file/vendor/xterm.css">
<script type="importmap">{ "imports": {
  "three": "/ts_dist/file/vendor/three.module.js",
  "@xterm/xterm": "/ts_dist/file/vendor/xterm.js",
  "@xterm/addon-fit": "/ts_dist/file/vendor/addon-fit.js"
}}</script>
</head><body>
<script type="module" src="/ts_dist/file/main.js"></script></body></html>`;
const MOUNT_FILE = join(DIST_DIR, "_persist_canvas.html");

// p1 seeded; its body runtime-persists p2 to the host store (the recipe path:
// `send web_loader {persist_record, record}`).
function seed(tmp: string): void {
  const canvasDir = join(tmp, ".fantastic", "web", "agents", "canvas");
  const p1 = join(canvasDir, "agents", "p1");
  mkdirSync(p1, { recursive: true });
  writeFileSync(
    join(canvasDir, "agent.json"),
    JSON.stringify({ id: "canvas", handler_module: "canvas.ts", display_name: "canvas" }),
  );
  const p1Body =
    `<pre id="out">P1-SEEDED</pre><script>` +
    `fantastic.send("web_loader", { type:"persist_record", record:{` +
    `id:"p2", handler_module:"html_agent.ts", parent_id:"canvas", display_name:"P2",` +
    `html:"<pre id=out>P2-RUNTIME</pre>", x:400, y:40, width:300, height:120 } });` +
    `</script>`;
  writeFileSync(
    join(p1, "agent.json"),
    JSON.stringify({ id: "p1", handler_module: "html_agent.ts", display_name: "P1", html: p1Body, x: 40, y: 40, width: 300, height: 120 }),
  );
}

const READ_OUTS =
  "(() => { const e = document.getElementById('out'); return e ? (e.textContent || '').trim() : ''; })()";

test("persistence: seeded + runtime-persisted records survive a daemon restart", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  let host: Host | null = null;
  let browser: Browser | null = null;
  try {
    host = await bootHost(8931, { webLoader: true, serveDist: true });
    seed(host.tmp);
    writeServedDist(host, "_persist_canvas.html", MOUNT_HTML); // into the dir actually served (local: the workdir copy)
    browser = await Browser.launch();

    // first boot: p1 renders + its script persists p2 to the host web_loader
    await browser.goto(`${host.httpOrigin}/ts_dist/file/_persist_canvas.html`);
    await browser.waitFor("document.querySelectorAll('.agent-frame iframe').length >= 1", 20000);
    const before = await browser.evalInAnyIframe<string>(READ_OUTS, 15000);
    assert.ok(before && /P1-SEEDED/.test(before), `seeded panel rendered first boot (got ${JSON.stringify(before)})`);
    // give the runtime persist_record time to land on host disk
    await new Promise((r) => setTimeout(r, 2500));
    assert.ok(
      existsSync(join(host.tmp, ".fantastic", "web", "agents", "canvas", "agents", "p2", "agent.json")),
      "runtime-persisted p2 record written to host disk",
    );

    // ── kill + re-spawn the daemon in the same dir ──
    await restartHost(host);

    // reload → the JS kernel re-hydrates the tree from the persisted store
    await browser.goto(`${host.httpOrigin}/ts_dist/file/_persist_canvas.html`);
    await browser.waitFor("document.querySelectorAll('.agent-frame iframe').length >= 2", 25000);
    const raw = await browser.evalAllIframes<string>(READ_OUTS);
    const texts = (Array.isArray(raw) ? raw : []).filter((x): x is string => typeof x === "string");
    const all = texts.join("|");
    assert.ok(/P1-SEEDED/.test(all), `seeded panel rehydrated after restart (got ${all})`);
    assert.ok(/P2-RUNTIME/.test(all), `runtime-persisted panel survived restart (got ${all})`);
    assert.deepEqual(
      browser.pageErrors.filter((e) => !/WebGL|GL context/i.test(e)),
      [],
      "no uncaught page errors across the restart",
    );
  } finally {
    if (browser !== null) browser.close();
    if (host !== null) teardownHost(host);
    try {
      rmSync(MOUNT_FILE, { force: true });
    } catch {
      /* best effort */
    }
  }
});
