// Revive the canvas ARTIFACT (`src/tui/web/canvas/dist/canvas.zip`) exactly as
// its readme says, in a real browser, and prove the single rolled-up
// bundle.min.js runs — three.js + xterm + xterm.css all INLINED, NO import map,
// NO external stylesheet, NO CDN.
//
// This is the runtime half of the pack story (the canvas `scripts/pack.sh` does
// the static half: single .js, no residual bare imports, the stamped sha). Here:
//   - the zip's readme carries the sha256 of the bundle it ships, and the bundle
//     the host serves matches it (verify before serving — it runs in a browser);
//   - the bundle's css-inject shim ran: a <style data-fantastic-vendor="xterm">
//     is in the document (xterm.css inlined via esbuild's text loader);
//   - a double click spawns a terminal whose inlined xterm.js renders a live
//     .xterm (the UMD globals survived esbuild's ESM wrapping);
//   - its exec_bridge terminal appears on the HOST disk, a child of the socket leg;
//   - no uncaught page errors (the one served file is self-sufficient).
//
// Run: cd integration_tests/py_ts && node --test --test-force-exit bundle_revive.browser.itest.ts

import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { bootHost, teardownHost, writeRecord, recordField, CANVAS_ZIP } from "./_host.ts";
import type { Host } from "./_host.ts";
import { Browser, chromeAvailable } from "./_chrome.ts";

function seedCanvas(tmp: string): void {
  writeRecord(join(tmp, ".fantastic", "web", "agents", "canvas"), {
    id: "canvas", handler_module: "canvas.ts", display_name: "canvas",
  });
}

/** The terminal backends on the host's disk whose folder sits under the leg's. */
function legBackends(tmp: string, leg: string): string[] {
  const out: string[] = [];
  const stack = [join(tmp, ".fantastic", "agents")];
  while (stack.length) {
    const d = stack.pop()!;
    let entries: string[] = [];
    try {
      entries = readdirSync(d);
    } catch {
      continue;
    }
    for (const e of entries) {
      const p = join(d, e);
      if (e !== "agent.yaml") {
        stack.push(p);
        continue;
      }
      const parts = p.split("/");
      if (parts.at(-4) === leg && recordField(p, "handler_module") === "exec_bridge.tools") out.push(p);
    }
  }
  return out;
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

test("canvas.zip revives: stamped sha matches, inlined css + xterm render against the host", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  let host: Host | null = null;
  let browser: Browser | null = null;
  try {
    host = await bootHost(8934, { webLoader: true, serveDist: true });
    seedCanvas(host.tmp);

    // the artifact's own integrity check, against the bytes actually served
    const readme = spawnSync("unzip", ["-p", CANVAS_ZIP, "readme.md"], { encoding: "utf8" }).stdout;
    const stamped = readme.match(/expected:\s*([0-9a-f]{64})/)?.[1];
    assert.ok(stamped, "the zip's readme carries the bundle's sha256");
    const served = createHash("sha256").update(readFileSync(join(host.tmp, "canvas_page", "bundle.min.js"))).digest("hex");
    assert.equal(served, stamped, "the served bundle is the one the readme stamps");

    browser = await Browser.launch();
    await browser.goto(host.pageUrl!);

    // the canvas shell mounted → the single bundle parsed + booted main.ts
    await browser.waitFor("!!document.getElementById('canvas')", 20000);

    // the css-inject shim ran at bundle eval → xterm.css is inlined (no <link>)
    assert.ok(
      await browser.evaluate<boolean>("!!document.querySelector('style[data-fantastic-vendor=\"xterm\"]')"),
      "bundle injected the inlined xterm.css (no external stylesheet)",
    );
    await sleep(800); // bridge + tree settle

    // dblclick → the declared backend + a bound terminal_view; the inlined
    // xterm.js (UMD globals) must render a live .xterm
    await browser.evaluate(
      "(() => { const c = document.getElementById('canvas'); c.dispatchEvent(new MouseEvent('dblclick', { bubbles: true, clientX: 420, clientY: 300 })); return true; })()",
    );
    // the double click opens the SPOT INPUT: pick `terminal` in it
    await browser.waitFor("!!document.querySelector('.action-popup input')", 10000);
    await browser.evaluate(
      "(() => { const i = document.querySelector('.action-popup input'); i.value = 'terminal'; i.dispatchEvent(new Event('input')); i.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true })); return true; })()",
    );
    await browser.waitFor("document.querySelectorAll('.agent-frame .xterm').length >= 1", 20000);
    assert.ok(
      await until(() => legBackends(host!.tmp, host!.webWsId).length === 1, 15000),
      "an exec_bridge terminal was created on the host, a child of the leg, through the bundle's bridge",
    );

    assert.deepEqual(
      browser.pageErrors.filter((e) => !/WebGL|GL context/i.test(e)),
      [],
      "no uncaught page errors — the one served file is self-sufficient",
    );
  } finally {
    if (browser !== null) browser.close();
    if (host !== null) teardownHost(host);
  }
});
