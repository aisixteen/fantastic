// Part-3 lifecycle: the canvas dblclick spawns a terminal by DISCOVERING a
// PTY-capable bundle from the host catalog (no hardcoded handler_module) and
// pairing a terminal_view to it; closing the frame removes BOTH — the frontend
// view AND the host backend it owns (cascade over the bridge).
//
// Proof points:
//   create: dblclick -> a terminal frame renders  +  a terminal_backend agent
//           appears on the HOST's on-disk tree (.fantastic/agents/**).
//   delete: close (×)  -> the frame disappears     +  that host backend dir is GONE.
//
// Run: (cd ts && npm run build) then:
//   cd integration_tests/py_ts && node --test --test-force-exit canvas_terminal.browser.itest.ts

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, writeFileSync, rmSync, existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { bootHost, teardownHost, DIST_DIR } from "./_host.ts";
import type { Host } from "./_host.ts";
import { Browser, chromeAvailable } from "./_chrome.ts";

const MOUNT_HTML = `<!doctype html><html><head><meta charset="utf-8">
<title>fantastic · canvas terminal test</title>
<link rel="stylesheet" href="/ts_dist/file/vendor/xterm.css">
<script type="importmap">{ "imports": {
  "three": "/ts_dist/file/vendor/three.module.js",
  "@xterm/xterm": "/ts_dist/file/vendor/xterm.js",
  "@xterm/addon-fit": "/ts_dist/file/vendor/addon-fit.js"
}}</script>
</head><body>
<script type="module" src="/ts_dist/file/main.js"></script></body></html>`;
const MOUNT_FILE = join(DIST_DIR, "_canvas_term.html");

function seedCanvas(tmp: string): void {
  const canvasDir = join(tmp, ".fantastic", "web", "agents", "canvas");
  mkdirSync(canvasDir, { recursive: true });
  writeFileSync(
    join(canvasDir, "agent.json"),
    JSON.stringify({ id: "canvas", handler_module: "canvas.ts", display_name: "canvas" }),
  );
}

// Walk the host's on-disk agent tree for a terminal_backend record.
function hostHasTerminalBackend(tmp: string): boolean {
  const root = join(tmp, ".fantastic", "agents");
  const stack = [root];
  while (stack.length) {
    const dir = stack.pop()!;
    let entries: string[] = [];
    try {
      entries = readdirSync(dir);
    } catch {
      continue;
    }
    if (entries.includes("agent.json")) {
      try {
        const rec = JSON.parse(readFileSync(join(dir, "agent.json"), "utf8")) as {
          handler_module?: string;
        };
        if (rec.handler_module === "terminal_backend.tools") return true;
      } catch {
        /* skip */
      }
    }
    for (const e of entries) {
      const p = join(dir, e);
      try {
        if (readdirSync(p)) stack.push(p);
      } catch {
        /* not a dir */
      }
    }
  }
  return false;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
async function until(cond: () => boolean, ms: number): Promise<boolean> {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (cond()) return true;
    await sleep(250);
  }
  return cond();
}

test("canvas dblclick spawns a terminal (discovered) + close removes both", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  let host: Host | null = null;
  let browser: Browser | null = null;
  try {
    host = await bootHost(8933, { webLoader: true, serveDist: true });
    seedCanvas(host.tmp);
    writeFileSync(MOUNT_FILE, MOUNT_HTML);
    browser = await Browser.launch();
    await browser.goto(`${host.httpOrigin}/ts_dist/file/_canvas_term.html`);
    await browser.waitFor("!!document.getElementById('canvas')", 20000);
    await sleep(800); // bridge + tree settle

    // ── CREATE: dblclick an empty spot ──
    await browser.evaluate(
      "(() => { const c = document.getElementById('canvas'); c.dispatchEvent(new MouseEvent('dblclick', { bubbles: true, clientX: 420, clientY: 300 })); return true; })()",
    );
    await browser.waitFor("document.querySelectorAll('.agent-frame').length >= 1", 20000);
    assert.ok(
      await until(() => hostHasTerminalBackend(host!.tmp), 15000),
      "a terminal_backend was created on the host (discovered from the catalog, not hardcoded)",
    );

    // ── DELETE: click the frame's close (×) ──
    await browser.evaluate(
      "(() => { const x = document.querySelector('.agent-frame .close'); if (!x) return false; x.click(); return true; })()",
    );
    await browser.waitFor("document.querySelectorAll('.agent-frame').length === 0", 20000);
    assert.ok(
      await until(() => !hostHasTerminalBackend(host!.tmp), 15000),
      "closing the owned terminal cascade-deleted the host backend (removes BOTH)",
    );

    assert.deepEqual(
      browser.pageErrors.filter((e) => !/WebGL|GL context/i.test(e)),
      [],
      "no uncaught page errors",
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
