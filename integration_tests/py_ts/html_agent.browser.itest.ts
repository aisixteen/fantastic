// Keystone: prove the DECOUPLED connector path end-to-end in a real browser,
// hand-written (no LLM), with NO bypass.
//
// Two html_agent panels. Panel1's button uses the injected `fantastic` connector
// to (a) `send` to a HOST `exec_bridge` (routed by the JS kernel over the
// kernel bridge) and show the value live, then (b) `emit` DIRECTLY to panel2 by
// id — a LOCAL JS-kernel fan-out, no host rendezvous. Panel2 `onMessage`s and
// renders it. The iframe talks ONLY to the JS kernel (postMessage); it has no host
// URL/WS. Headless Chrome clicks Run and asserts BOTH panels light up.
//
// Panel1 SENDS AS ITSELF: its own record grants it `start` + `watch` on the
// exec leg and `emit` onto panel2 (which accepts it by name), and the exec leg runs only `<python> -c <code>` (its `allowed`).
//
// Run: npm run test:integration  (skips without Chrome/.venv; the harness packs
// the canvas app itself).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { bootHost, teardownHost, writeRecord } from "./_host.ts";
import type { Host } from "./_host.ts";
import { Browser, chromeAvailable } from "./_chrome.ts";

let host: Host | null = null;
let browser: Browser | null = null;
let skipReason = "";

// Panel1: Run button → `fantastic.send` to the host exec_bridge (over the
// kernel bridge) → live value, then `fantastic.emit` DIRECT to panel2 by id.
function panel1Body(pyId: string, python: string): string {
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
    command:${JSON.stringify(python)}, args:["-c", "import random;print('val-'+str(random.randint(1000,9999)))"] });
  if (!r || !r.job_id) { document.getElementById("out").textContent = "REFUSED " + JSON.stringify(r); return; }
  curJid = r.job_id;
  if (document.getElementById("out").textContent === "—") document.getElementById("out").textContent = "started " + r.job_id;
};
</script>`;
}

// Panel2: renders whatever is sent to its own id (the JS kernel delivers panel1's
// emit to panel2's inbox → connector.onMessage). No host, no rendezvous.
const PANEL2_BODY = `<pre id="got">—</pre>
<script>
fantastic.onMessage((p) => { if (p && p.type === "value") document.getElementById("got").textContent = p.value; });
</script>`;

function seedTree(tmp: string, pyId: string, python: string): void {
  const canvasDir = join(tmp, ".fantastic", "web", "agents", "canvas");
  writeRecord(canvasDir, { id: "canvas", handler_module: "canvas.ts", display_name: "canvas" });
  writeRecord(join(canvasDir, "agents", "panel1"), {
    id: "panel1", handler_module: "html_agent.ts", display_name: "PANEL-1", html: panel1Body(pyId, python),
    // a sibling emit is gated both sides under `emit`, so the pair names it
    can_call: { [pyId]: ["start", "watch"], panel2: ["emit"] },
  });
  writeRecord(join(canvasDir, "agents", "panel2"), {
    id: "panel2", handler_module: "html_agent.ts", display_name: "PANEL-2", html: PANEL2_BODY,
    callable_by: { panel1: ["emit"] },
  });
}

before(async () => {
  if (!chromeAvailable()) {
    skipReason = "system Chrome not found";
    return;
  }
  try {
    host = await bootHost(8915, { webLoader: true, serveDist: true, execBridge: true });
    seedTree(host.tmp, host.pyId!, host.python!);
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
});

test("html_agent connector: button → host python (via JS kernel) + DIRECT panel1→panel2", async (t) => {
  if (host === null || browser === null) return t.skip(skipReason);
  const b = browser;

  await b.goto(host.pageUrl!);

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
    "(() => { const e = document.getElementById('out'); return e && /^val-/.test(e.textContent) ? e.textContent : null; })()",
    15000,
  );
  const panel = await b.evalInAnyIframe<string>(
    "(() => { const e = document.getElementById('out'); return e ? e.textContent : null; })()",
    1000,
  );
  assert.ok(
    out && /^val-\d{4}$/.test(out),
    `Panel1 live value from exec_bridge (got ${JSON.stringify(out)}; panel shows ${JSON.stringify(panel)}; page: ${b.pageErrors.join(" | ")})`,
  );

  // Panel2 got it via a DIRECT panel1→panel2 emit (local JS-kernel fan-out, no relay)
  const got = await b.evalInAnyIframe<string>(
    "(() => { const e = document.getElementById('got'); return e && e.textContent !== '—' ? e.textContent : null; })()",
    15000,
  );
  assert.equal(got, out, "Panel2 received Panel1's value directly by id");

  assert.deepEqual(browser.pageErrors, [], "no uncaught page errors during the flow");
});
