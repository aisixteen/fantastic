// Revive the SOVEREIGN artifact (ts/dist/js_kernel.zip) in a real browser and
// prove the single rolled-up bundle.min.js actually runs — three.js + xterm +
// xterm.css all INLINED, NO import map, NO external stylesheet, NO CDN.
//
// This is the runtime half of the pack story (scripts/pack.sh does the static
// half: single .js, no residual bare imports, sha integrity). Here we serve the
// EXACT bundle bytes through the host's generic `file` agent — exactly the
// revive recipe in ts/readme.md — and mount a terminal against it.
//
// Proof points:
//   - the bundle's css-inject shim ran: a <style data-fantastic-vendor="xterm">
//     is in the document (xterm.css was inlined via esbuild's text loader).
//   - dblclick spawns a terminal whose inlined xterm.js renders a live .xterm
//     (the UMD globals survived esbuild's ESM wrapping).
//   - a terminal_backend appears on the HOST disk (the bridge + pairing work
//     through the bundle just like the scattered-module build).
//   - no uncaught page errors (the one served file is self-sufficient).
//
// Prereq: (cd ts && npm run build && sh scripts/pack.sh). Run:
//   cd integration_tests/py_ts && node --test --test-force-exit bundle_revive.browser.itest.ts

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, writeFileSync, rmSync, existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { bootHost, teardownHost, DIST_DIR } from "./_host.ts";
import type { Host } from "./_host.ts";
import { Browser, chromeAvailable } from "./_chrome.ts";

// The revive mount page: ONE script tag, nothing else. No import map (vendors
// inlined), no <link rel=stylesheet> (xterm.css inlined + injected by the shim).
const MOUNT_HTML = `<!doctype html><html><head><meta charset="utf-8">
<title>fantastic · js_kernel.zip revive</title></head><body>
<script type="module" src="/ts_dist/file/pack/bundle.min.js"></script></body></html>`;
const MOUNT_FILE = join(DIST_DIR, "_bundle_revive.html");
const BUNDLE = join(DIST_DIR, "pack", "bundle.min.js");

function seedCanvas(tmp: string): void {
  const canvasDir = join(tmp, ".fantastic", "web", "agents", "canvas");
  mkdirSync(canvasDir, { recursive: true });
  writeFileSync(
    join(canvasDir, "agent.json"),
    JSON.stringify({ id: "canvas", handler_module: "canvas.ts", display_name: "canvas" }),
  );
}

function hostHasTerminalBackend(tmp: string): boolean {
  const stack = [join(tmp, ".fantastic", "agents")];
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

test("js_kernel.zip bundle revives: inlined css + xterm render against the host", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(BUNDLE)) return t.skip("bundle not packed — run `sh scripts/pack.sh` in ts/");
  let host: Host | null = null;
  let browser: Browser | null = null;
  try {
    host = await bootHost(8934, { webLoader: true, serveDist: true });
    seedCanvas(host.tmp);
    writeFileSync(MOUNT_FILE, MOUNT_HTML);
    browser = await Browser.launch();
    await browser.goto(`${host.httpOrigin}/ts_dist/file/_bundle_revive.html`);

    // the canvas shell mounted → the single bundle parsed + booted main.ts
    await browser.waitFor("!!document.getElementById('canvas')", 20000);

    // the css-inject shim ran at bundle eval → xterm.css is inlined (no <link>)
    assert.ok(
      await browser.evaluate<boolean>(
        "!!document.querySelector('style[data-fantastic-vendor=\"xterm\"]')",
      ),
      "bundle injected the inlined xterm.css (no external stylesheet)",
    );
    await sleep(800); // bridge + tree settle

    // dblclick → discover a PTY bundle + pair a terminal_view; the inlined
    // xterm.js (UMD globals) must render a live .xterm
    await browser.evaluate(
      "(() => { const c = document.getElementById('canvas'); c.dispatchEvent(new MouseEvent('dblclick', { bubbles: true, clientX: 420, clientY: 300 })); return true; })()",
    );
    await browser.waitFor("document.querySelectorAll('.agent-frame').length >= 1", 20000);
    await browser.waitFor("document.querySelectorAll('.xterm').length >= 1", 20000);
    assert.ok(
      await until(() => hostHasTerminalBackend(host!.tmp), 15000),
      "a terminal_backend was created on the host through the bundle's bridge",
    );

    assert.deepEqual(
      browser.pageErrors.filter((e) => !/WebGL|GL context/i.test(e)),
      [],
      "no uncaught page errors — the one served file is self-sufficient",
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
