// Keystone: prove the DECOUPLED connector path end-to-end in a real browser,
// hand-written (no LLM), with NO bypass.
//
// Two html_agent panels. Panel1's button uses the injected `fantastic` connector
// to (a) `send` to a HOST `python_runtime` (routed by the JS kernel over the
// kernel bridge) and show the value live, then (b) `emit` DIRECTLY to panel2 by
// id — a LOCAL JS-kernel fan-out, no host rendezvous. Panel2 `onMessage`s and
// renders it. The iframe talks ONLY to the JS kernel (postMessage); it has no host
// URL/WS. Headless Chrome clicks Run and asserts BOTH panels light up.
//
// Run: npm run build && npm run test:integration  (skips without Chrome/.venv or
// an unbuilt ts/dist).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { join } from "node:path";
import { bootHost, teardownHost, DIST_DIR, writeServedDist } from "./_host.ts";
import type { Host } from "./_host.ts";
import { Browser, chromeAvailable } from "./_chrome.ts";

let host: Host | null = null;
let browser: Browser | null = null;
let skipReason = "";

const MOUNT_HTML = `<!doctype html><html><head><meta charset="utf-8">
<title>fantastic · canvas · html_agent test</title>
<link rel="stylesheet" href="/ts_dist/file/vendor/xterm.css">
<script type="importmap">{ "imports": {
  "three": "/ts_dist/file/vendor/three.module.js",
  "@xterm/xterm": "/ts_dist/file/vendor/xterm.js",
  "@xterm/addon-fit": "/ts_dist/file/vendor/addon-fit.js"
}}</script>
</head><body>
<script type="module" src="/ts_dist/file/main.js"></script>
</body></html>`;
const MOUNT_FILE = join(DIST_DIR, "_test_canvas.html");

// Panel1: Run button → `fantastic.send` to the host python_runtime (over the
// kernel bridge) → live value, then `fantastic.emit` DIRECT to panel2 by id.
function panel1Body(pyId: string): string {
  return `<button id="run">Run</button> <pre id="out" style="display:inline">—</pre>
<script>
let curJid = null;
fantastic.watch(${JSON.stringify(pyId)}, (ev) => {           // live progress events from the host job
  if (!ev || ev.job_id !== curJid || ev.type !== "progress" || ev.stream !== "stdout") return;
  document.getElementById("out").textContent = ev.line;      // live, in place
  fantastic.emit("panel2", { type:"value", value: ev.line }); // DIRECT → panel2, no relay
});
document.getElementById("run").onclick = async () => {
  const r = await fantastic.send(${JSON.stringify(pyId)}, { type:"start",   // async, non-blocking
    code:"import random;print('val-'+str(random.randint(1000,9999)))" });
  curJid = r.job_id;
};
</script>`;
}

// Panel2: renders whatever is sent to its own id (the JS kernel delivers panel1's
// emit to panel2's inbox → connector.onMessage). No host, no rendezvous.
const PANEL2_BODY = `<pre id="got">—</pre>
<script>
fantastic.onMessage((p) => { if (p && p.type === "value") document.getElementById("got").textContent = p.value; });
</script>`;

function seedTree(tmp: string, pyId: string): void {
  const agents = join(tmp, ".fantastic", "web", "agents");
  const canvasDir = join(agents, "canvas");
  const p1 = join(canvasDir, "agents", "panel1");
  const p2 = join(canvasDir, "agents", "panel2");
  mkdirSync(p1, { recursive: true });
  mkdirSync(p2, { recursive: true });
  writeFileSync(
    join(canvasDir, "agent.json"),
    JSON.stringify({ id: "canvas", handler_module: "canvas.ts", display_name: "canvas" }),
  );
  writeFileSync(
    join(p1, "agent.json"),
    JSON.stringify({ id: "panel1", handler_module: "html_agent.ts", display_name: "PANEL-1", html: panel1Body(pyId) }),
  );
  writeFileSync(
    join(p2, "agent.json"),
    JSON.stringify({ id: "panel2", handler_module: "html_agent.ts", display_name: "PANEL-2", html: PANEL2_BODY }),
  );
}

before(async () => {
  if (!chromeAvailable()) {
    skipReason = "system Chrome not found";
    return;
  }
  if (!existsSync(join(DIST_DIR, "main.js"))) {
    skipReason = "ts/dist not built — run `npm run build`";
    return;
  }
  try {
    host = await bootHost(8915, { webLoader: true, serveDist: true, pythonRuntime: true });
    seedTree(host.tmp, host.pyId ?? "python_runtime");
    writeServedDist(host, "_test_canvas.html", MOUNT_HTML); // into the dir actually served (local: the workdir copy)
    browser = await Browser.launch();
  } catch (e) {
    skipReason = `browser host unavailable: ${(e as Error).message}`;
    if (host !== null) teardownHost(host);
    host = null;
  }
});

after(() => {
  if (browser !== null) browser.close();
  if (host !== null) teardownHost(host);
  try {
    rmSync(MOUNT_FILE, { force: true });
  } catch {
    /* best effort */
  }
});

test("html_agent connector: button → host python (via JS kernel) + DIRECT panel1→panel2", async (t) => {
  if (host === null || browser === null) return t.skip(skipReason);
  const b = browser;

  await b.goto(`${host.httpOrigin}/ts_dist/file/_test_canvas.html`);

  // both panels hydrated + rendered as iframes
  await b.waitFor(
    "!!document.getElementById('canvas') && document.querySelectorAll('.agent-frame iframe').length >= 2",
    20000,
  );

  // click Panel1's Run button (inside its null-origin srcdoc iframe, via CDP)
  const clicked = await b.clickInAnyIframe("#run", 15000);
  assert.ok(clicked, "Panel1 Run button found + clicked");

  // Panel1 shows the host-computed value (send → JS kernel → kernel bridge → host → reply)
  const out = await b.evalInAnyIframe<string>(
    "(() => { const e = document.getElementById('out'); return e && e.textContent !== '—' ? e.textContent : null; })()",
    15000,
  );
  assert.ok(out && /^val-\d{4}$/.test(out), `Panel1 live value from python_runtime (got ${JSON.stringify(out)})`);

  // Panel2 got it via a DIRECT panel1→panel2 emit (local JS-kernel fan-out, no relay)
  const got = await b.evalInAnyIframe<string>(
    "(() => { const e = document.getElementById('got'); return e && e.textContent !== '—' ? e.textContent : null; })()",
    15000,
  );
  assert.equal(got, out, "Panel2 received Panel1's value directly by id");

  assert.deepEqual(browser.pageErrors, [], "no uncaught page errors during the flow");
});
