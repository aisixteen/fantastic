// Verify a system an LLM assembled FROM THE ZIP README ALONE actually runs the
// canvas_terminal integration scenario. Points at an ALREADY-RUNNING daemon (the
// builder agent left it up) — does NOT boot its own host. Proves the readme-only
// revive produced a CONNECTED py+js system, not just a served file.
//
//   usage: node e2e/revive_verify.ts <CANVAS_URL> <WORKDIR>
//     CANVAS_URL — the mount page the builder created (loads bundle.min.js)
//     WORKDIR    — the daemon's cwd, so we can read its on-disk .fantastic tree
//
// PASS iff: canvas mounts · inlined xterm.css injected · dblclick spawns a live
// .xterm · a terminal_backend appears on the HOST disk · no uncaught page errors.
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { Browser, chromeAvailable } from "../_chrome.ts";

const canvasUrl = process.argv[2];
const workdir = process.argv[3];
if (!canvasUrl || !workdir) {
  console.error("usage: node e2e/revive_verify.ts <CANVAS_URL> <WORKDIR>");
  process.exit(2);
}
if (!chromeAvailable()) {
  console.log("SKIP: system Chrome not found");
  process.exit(0);
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
      try {
        if (readdirSync(join(dir, e))) stack.push(join(dir, e));
      } catch {
        /* not a dir */
      }
    }
  }
  return false;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
async function until(cond: () => boolean, ms: number): Promise<boolean> {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (cond()) return true;
    await sleep(250);
  }
  return cond();
}

const b = await Browser.launch();
const checks: [string, boolean][] = [];
try {
  await b.goto(canvasUrl);

  await b.waitFor("!!document.getElementById('canvas')", 20000);
  checks.push(["canvas mounted (bundle booted main.ts)", true]);

  const cssInjected = await b.evaluate<boolean>(
    "!!document.querySelector('style[data-fantastic-vendor=\"xterm\"]')",
  );
  checks.push(["inlined xterm.css injected (no external <link>)", cssInjected]);
  await sleep(800); // bridge + tree settle

  await b.evaluate(
    "(() => { const c = document.getElementById('canvas'); c.dispatchEvent(new MouseEvent('dblclick', { bubbles: true, clientX: 420, clientY: 300 })); return true; })()",
  );
  await b.waitFor("document.querySelectorAll('.agent-frame').length >= 1", 20000);
  const xtermRendered = await b
    .waitFor("document.querySelectorAll('.xterm').length >= 1", 20000)
    .then(() => true)
    .catch(() => false);
  checks.push(["inlined xterm.js rendered a live .xterm", xtermRendered]);

  const backendOnDisk = await until(() => hostHasTerminalBackend(workdir), 15000);
  checks.push(["host created a terminal_backend over the bridge", backendOnDisk]);

  const realErrors = b.pageErrors.filter((e) => !/WebGL|GL context/i.test(e));
  checks.push(["no uncaught page errors", realErrors.length === 0]);
  if (realErrors.length) console.log("page errors:", JSON.stringify(realErrors));
} finally {
  b.close();
}

console.log("=== revive verification (readme-only assembled system) ===");
for (const [name, ok] of checks) console.log(`  ${ok ? "PASS" : "FAIL"}  ${name}`);
const allPass = checks.every(([, ok]) => ok);
console.log(allPass ? "\nRESULT: PASS" : "\nRESULT: FAIL");
process.exit(allPass ? 0 : 1);
