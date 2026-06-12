// Browser E2E — the two-tree round-trip through a REAL browser runtime. A live
// python daemon serves the built frontend (`ts/dist`) + the `web_loader` WS; a
// headless Chrome loads `main.js`, which dials the host over its NATIVE
// WebSocket, hydrates the canvas's member tree from `.fantastic/web/`, and
// renders it to the DOM. This covers the layer the Node itests can't: the actual
// browser, its own WebSocket, and `mountCanvas`'s DOM render.
//
// We seed a tree on the host disk (canvas + one `ai_view` member), open the
// page, and assert via CDP that the canvas shell mounted and a frame for the
// seeded member rendered — i.e. it crossed the wire and hydrated.
//
// Run: npm run build && npm run test:integration   (skips without Chrome/.venv;
// also skips if `ts/dist` isn't built).

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

// A minimal mount page (the build emits no HTML): the import map binds the bare
// three/xterm specifiers to the vendored ESM, then loads the canvas bootstrap.
const MOUNT_HTML = `<!doctype html><html><head><meta charset="utf-8">
<title>fantastic · canvas · test</title>
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

/** Seed a frontend tree on the host disk under `.fantastic/web/` (the
 *  `web_loader` namespace) — read fresh on the browser's `load_tree`. */
function seedWebTree(tmp: string): void {
  const agents = join(tmp, ".fantastic", "web", "agents");
  const canvasDir = join(agents, "canvas");
  const noteDir = join(canvasDir, "agents", "note");
  mkdirSync(noteDir, { recursive: true });
  writeFileSync(
    join(canvasDir, "agent.json"),
    JSON.stringify({ id: "canvas", handler_module: "canvas.ts", display_name: "canvas" }),
  );
  writeFileSync(
    join(noteDir, "agent.json"),
    JSON.stringify({
      id: "note",
      handler_module: "terminal_view.ts", // mounts an empty xterm cleanly w/o a backend
      display_name: "NOTE-ALPHA",
    }),
  );
}

before(async () => {
  if (!chromeAvailable()) {
    skipReason = "system Chrome not found";
    return;
  }
  if (!existsSync(join(DIST_DIR, "main.js"))) {
    skipReason = "ts/dist not built — run `npm run build` first";
    return;
  }
  try {
    host = await bootHost(8914, { webLoader: true, serveDist: true });
    seedWebTree(host.tmp);
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

test("browser hydrates the seeded canvas tree over a native WebSocket + renders it", async (t) => {
  if (host === null || browser === null) return t.skip(skipReason);
  const b = browser;

  // load the real frontend; main.js dials the host over its OWN WebSocket
  await b.goto(`${host.httpOrigin}/ts_dist/file/_test_canvas.html`);

  // wait until the canvas shell mounted AND a member frame rendered — reaching
  // this state proves: boot → WS connect → load_tree → kernel.load → mountCanvas
  await b.waitFor(
    "!!document.getElementById('canvas') && document.querySelectorAll('.agent-frame').length >= 1",
    20000,
  );

  const dom = await b.evaluate<{
    shell: boolean;
    frames: string[];
    headText: string;
  }>(`(() => ({
    shell: !!document.getElementById('canvas') && !!document.getElementById('toolbar')
           && !!document.querySelector('.canvas-world'),
    frames: [...document.querySelectorAll('.agent-frame code')].map(c => c.textContent),
    headText: document.querySelector('.agent-frame .frame-head')?.textContent || '',
  }))()`);

  assert.ok(dom.shell, "mountCanvas built the canvas shell (#canvas/#toolbar/.canvas-world)");
  assert.ok(
    dom.frames.includes("note"),
    `the seeded 'note' member hydrated + rendered a frame (got ${JSON.stringify(dom.frames)})`,
  );
  assert.match(dom.headText, /NOTE-ALPHA/, "the member's display_name reached the DOM");
  assert.deepEqual(browser.pageErrors, [], "no uncaught page errors during boot");
});
