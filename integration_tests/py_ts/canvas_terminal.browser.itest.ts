// The canvas pair lifecycle against a python host: a double click spawns the
// backend the terminal view DECLARES it fronts (`exec_bridge.tools`, looked
// up by that exact name in the host catalog) as a CHILD OF THE SOCKET LEG,
// carrying its view's and canvas's ids; the view is bound to it and granted
// exactly it. Closing the frame removes BOTH — the view and the backend.
//
// Proof points, on the page AND on the host's disk:
//   create: dblclick -> a terminal frame renders  +  an exec_bridge terminal record
//           appears under the leg (`…/<web_ws>/agents/<id>/agent.yaml`) with
//           `view_id` + `canvas_id`, and the view's folder under the canvas.
//   delete: close (×)  -> the frame disappears     +  both folders are GONE.
//
// Run: cd integration_tests/py_ts && node --test --test-force-exit canvas_terminal.browser.itest.ts

import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { bootHost, teardownHost, writeRecord, recordField } from "./_host.ts";
import type { Host } from "./_host.ts";
import { Browser, chromeAvailable } from "./_chrome.ts";

function seedCanvas(tmp: string): void {
  writeRecord(join(tmp, ".fantastic", "web", "agents", "canvas"), {
    id: "canvas", handler_module: "canvas.ts", display_name: "canvas",
  });
}

/** Every `agent.yaml` under `dir`, recursively. */
function records(dir: string): string[] {
  const out: string[] = [];
  const stack = [dir];
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
      if (e === "agent.yaml") out.push(p);
      else stack.push(p);
    }
  }
  return out;
}

/** The host's terminal backends: `{file, parent dir name, view_id, canvas_id}`. */
function backends(tmp: string) {
  return records(join(tmp, ".fantastic", "agents"))
    .filter((f) => recordField(f, "handler_module") === "exec_bridge.tools")
    .map((f) => ({
      file: f,
      under: f.split("/").slice(-4, -3)[0],
      view: recordField(f, "view_id"),
      canvas: recordField(f, "canvas_id"),
    }));
}

/** The canvas's cells — every child folder but its singleton `background`. */
function views(tmp: string): string[] {
  const d = join(tmp, ".fantastic", "web", "agents", "canvas", "agents");
  return existsSync(d) ? readdirSync(d).filter((n) => n !== "background") : [];
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

test("canvas dblclick spawns a paired terminal under the leg + close removes both", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  let host: Host | null = null;
  let browser: Browser | null = null;
  try {
    host = await bootHost(8933, { webLoader: true, serveDist: true });
    seedCanvas(host.tmp);
    browser = await Browser.launch();
    await browser.goto(host.pageUrl!);
    await browser.waitFor("!!document.getElementById('canvas')", 20000);
    await sleep(800); // bridge + tree settle

    // ── CREATE: dblclick an empty spot ──
    await browser.evaluate(
      "(() => { const c = document.getElementById('canvas'); c.dispatchEvent(new MouseEvent('dblclick', { bubbles: true, clientX: 420, clientY: 300 })); return true; })()",
    );
    // the double click opens the SPOT INPUT: pick `terminal` in it
    await browser.waitFor("!!document.querySelector('.spot-input input')", 10000);
    await browser.evaluate(
      "(() => { const i = document.querySelector('.spot-input input'); i.value = 'terminal'; i.dispatchEvent(new Event('input')); i.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true })); return true; })()",
    );
    await browser.waitFor("!!document.querySelector('.agent-frame .xterm')", 20000);
    assert.ok(await until(() => backends(host!.tmp).length === 1, 15000), "one exec_bridge terminal on the host");
    const [b] = backends(host.tmp);
    assert.equal(b!.under, host.webWsId, "the backend is a CHILD OF THE LEG");
    assert.equal(b!.canvas, "canvas", "it carries its canvas's id");
    assert.ok(await until(() => views(host!.tmp).length === 1, 10000), "the view is a folder under the canvas");
    assert.equal(b!.view, views(host.tmp)[0], "the backend carries its view's id");
    const view = join(host.tmp, ".fantastic", "web", "agents", "canvas", "agents", views(host.tmp)[0]!, "agent.yaml");
    const backendId = b!.file.split("/").slice(-2, -1)[0]!;
    assert.equal(recordField(view, "backend_id"), backendId, "the view is bound to that backend");

    // ── DELETE: click the frame's close (×) ──
    await browser.evaluate(
      "(() => { const x = document.querySelector('.agent-frame .close'); if (!x) return false; x.click(); return true; })()",
    );
    await browser.waitFor("document.querySelectorAll('.agent-frame').length === 0", 20000);
    assert.ok(await until(() => backends(host!.tmp).length === 0, 15000), "the owned backend is deleted with its view");
    assert.ok(await until(() => views(host!.tmp).length === 0, 10000), "the view folder is gone");

    assert.deepEqual(
      browser.pageErrors.filter((e) => !/WebGL|GL context/i.test(e)),
      [],
      "no uncaught page errors",
    );
  } finally {
    if (browser !== null) browser.close();
    if (host !== null) teardownHost(host);
  }
});
