// Phase-2 DIAGNOSTIC: connect a real LLM (gpt-oss:120b-cloud via the local
// signed-in ollama) to a live daemon and see whether — from the readme +
// reflect ALONE — it can wire the same interactive demo the hand-written
// html_agent.browser.itest proves by construction. This is NOT a pass/fail gate:
// it RUNS the LLM, captures the tool-call trace + the resulting trees + a
// best-effort browser check, and writes a findings report. Answers: (1) is the
// readme enough? (2) can the LLM do the wiring? (3) do the primitives work?
//
// Run: npm run build && npm run test:integration  (skips without Chrome / .venv /
// a reachable gpt-oss:120b-cloud).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { writeFileSync, existsSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { bootHost, teardownHost, DIST_DIR } from "./_host.ts";
import type { Host } from "./_host.ts";
import { Browser, chromeAvailable } from "./_chrome.ts";

let host: Host | null = null;
let browser: Browser | null = null;
let skipReason = "";

const REPORT_FILE = join(DIST_DIR, "..", "tests", "llm_diag.out.md");
const PROMPT = [
  "You're connected to a Fantastic kernel as an agent that can call other agents",
  "by sending them messages. FIRST, read the kernel's own documentation — there is",
  "a one-call way to fetch the full readme (a reflect call). Study it so you",
  "understand how this system works: panels/the browser frontend, running",
  "background Python, and agent-to-agent messaging.",
  "",
  "THEN build a small interactive demo that shows up on the canvas:",
  "1) a web panel with a button labeled 'Run';",
  "2) clicking Run runs a tiny Python snippet in the background (compute something",
  "   simple, e.g. a random number) and shows that fresh value live inside the",
  "   panel, updating in place on each click;",
  "3) a SECOND web panel, and the first panel sends its computed value directly to",
  "   the second panel, point-to-point, addressed by the second panel's id.",
  "",
  "Discover the exact verbs yourself from the readme and your reflect calls — they",
  "are not given here. When finished, briefly list the agent ids you created and",
  "how you wired them.",
].join("\n");

async function openWs(url: string): Promise<WebSocket> {
  const ws = new WebSocket(url);
  await new Promise<void>((res, rej) => {
    ws.addEventListener("open", () => res(), { once: true });
    ws.addEventListener("error", () => rej(new Error(`ws open failed: ${url}`)), { once: true });
  });
  return ws;
}

interface LlmRun {
  final: Record<string, unknown> | null;
  events: Record<string, unknown>[];
}

/** Drive the ollama agent's `send` over WS, collecting its emitted events
 *  (status/token — they emit on the agent inbox for a non-"cli" client_id). */
async function runLLM(origin: string, ollamaId: string, text: string, ms: number): Promise<LlmRun> {
  const ws = await openWs(`${origin}/${ollamaId}/ws`);
  const events: Record<string, unknown>[] = [];
  let resolveDone: (v: Record<string, unknown> | null) => void = () => {};
  let rejectDone: (e: Error) => void = () => {};
  const done = new Promise<Record<string, unknown> | null>((res, rej) => {
    resolveDone = res;
    rejectDone = rej;
  });
  ws.addEventListener("message", (e: MessageEvent) => {
    let f: { type?: string; payload?: Record<string, unknown>; data?: Record<string, unknown>; error?: unknown };
    try {
      f = JSON.parse(String(e.data));
    } catch {
      return;
    }
    if (f.type === "event" && f.payload) events.push(f.payload);
    else if (f.type === "reply") resolveDone(f.data ?? null);
    else if (f.type === "error") rejectDone(new Error(String(f.error)));
  });
  ws.send(
    JSON.stringify({
      type: "call",
      target: ollamaId,
      payload: { type: "send", text, client_id: "diag" },
      id: "send-1",
    }),
  );
  const timer = setTimeout(() => rejectDone(new Error(`LLM send timed out after ${ms}ms`)), ms);
  try {
    const final = await done;
    return { final, events };
  } finally {
    clearTimeout(timer);
    ws.close();
  }
}

/** One-shot WS call (for querying trees after the run). */
async function wsCall(origin: string, target: string, payload: Record<string, unknown>, ms = 20000): Promise<unknown> {
  const ws = await openWs(`${origin}/${target}/ws`);
  try {
    return await new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("wsCall timeout")), ms);
      ws.addEventListener("message", (e: MessageEvent) => {
        let f: { type?: string; id?: string; data?: unknown };
        try {
          f = JSON.parse(String(e.data));
        } catch {
          return;
        }
        if (f.type === "reply" && f.id === "q1") {
          clearTimeout(timer);
          resolve(f.data);
        }
      });
      ws.send(JSON.stringify({ type: "call", target, payload, id: "q1" }));
    });
  } finally {
    ws.close();
  }
}

before(async () => {
  if (!chromeAvailable()) return void (skipReason = "system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return void (skipReason = "ts/dist not built");
  try {
    host = await bootHost(8916, {
      webLoader: true,
      serveDist: true,
      llm: { bundle: "anthropic_backend.tools", model: "claude-opus-4-8" },
    });
    // seed the canvas ROOT in the web_loader store (the operator step from the
    // readme web-setup recipe). Panels parent to it; without a `canvas/agent.json`
    // the loader can't walk its children → orphaned. The LLM only ADDS panels.
    const canvasDir = join(host.tmp, ".fantastic", "web", "agents", "canvas");
    mkdirSync(canvasDir, { recursive: true });
    writeFileSync(
      join(canvasDir, "agent.json"),
      JSON.stringify({
        id: "canvas",
        handler_module: "canvas.ts",
        sentence: "Canvas compositor — renders the frontend's own member tree.",
      }),
    );
    // write the canvas mount page used by the browser check
    writeFileSync(
      join(DIST_DIR, "_test_canvas.html"),
      `<!doctype html><html><head><meta charset="utf-8"><title>fantastic·canvas</title>
<link rel="stylesheet" href="/ts_dist/file/vendor/xterm.css">
<script type="importmap">{ "imports": {
  "three": "/ts_dist/file/vendor/three.module.js",
  "@xterm/xterm": "/ts_dist/file/vendor/xterm.js",
  "@xterm/addon-fit": "/ts_dist/file/vendor/addon-fit.js" }}</script>
</head><body><script type="module" src="/ts_dist/file/main.js"></script></body></html>`,
    );
    browser = await Browser.launch();
  } catch (e) {
    skipReason = `llm host unavailable: ${(e as Error).message}`;
    if (host !== null) teardownHost(host);
    host = null;
  }
});

after(() => {
  if (browser !== null) browser.close();
  if (host !== null) teardownHost(host);
});

test("DIAGNOSTIC: can claude-opus-4-8 wire the panel demo from the readme alone?", async (t) => {
  if (host === null || browser === null) return t.skip(skipReason);
  const h = host;
  const lines: string[] = [];
  const log = (s: string): void => {
    lines.push(s);
  };

  log(`# LLM diagnostic — claude-opus-4-8 (anthropic_backend)\n`);
  log(`ollama agent: \`${h.ollamaId}\` · python_runtime: \`${h.pyId}\`\n`);

  // ── drive the LLM ──
  const t0 = Date.now();
  let run: LlmRun;
  try {
    run = await runLLM(h.origin, h.ollamaId ?? "", PROMPT, 200000);
  } catch (e) {
    log(`\n**LLM run errored/timed out:** ${(e as Error).message}`);
    run = { final: null, events: [] };
  }
  const wall = ((Date.now() - t0) / 1000).toFixed(1);

  // ── build an ordered tool-call trace: merge tool_calling entry+exit by
  //    call_id so each step shows verb + args + reply preview ──
  interface Step {
    n: number;
    target: string;
    verb: string;
    args: string;
    reply: string;
  }
  const byCall = new Map<string, Step>();
  let order = 0;
  const readmeOn: string[] = [];
  for (const ev of run.events) {
    const detail = (ev["detail"] ?? {}) as Record<string, unknown>;
    const tool = (detail["tool"] ?? ev["tool"]) as
      | { call_id?: string; target?: string; verb?: string; args?: unknown; reply_preview?: unknown }
      | undefined;
    if (!tool || !tool.verb) continue;
    const cid = String(tool.call_id ?? `auto-${order}`);
    const argsStr = tool.args !== undefined ? JSON.stringify(tool.args) : "";
    if (!byCall.has(cid)) {
      byCall.set(cid, {
        n: ++order,
        target: tool.target ?? "?",
        verb: tool.verb,
        args: argsStr.slice(0, 200),
        reply: "",
      });
    }
    const step = byCall.get(cid) as Step;
    if (argsStr && step.args === "") step.args = argsStr.slice(0, 200);
    if (tool.reply_preview !== undefined) {
      step.reply = JSON.stringify(tool.reply_preview).slice(0, 200);
    }
    if (tool.verb === "reflect" && /readme/.test(argsStr)) readmeOn.push(tool.target ?? "?");
  }
  const trace = [...byCall.values()].sort((a, b) => a.n - b.n);
  const verbs = trace.map((x) => `${x.target}.${x.verb}`);
  const fetchedReadme = readmeOn.length > 0;
  const usedPersist = verbs.some((v) => v.includes("persist_record"));
  const usedHostCreate = verbs.some((v) => v.endsWith(".create_agent"));

  log(`\n## What the LLM did (${wall}s, ${run.events.length} events, ${trace.length} tool calls)`);
  log(`- fetched the readme (reflect readme=true): **${fetchedReadme}**${fetchedReadme ? ` — on ${[...new Set(readmeOn)].join(", ")}` : ""}`);
  log(`- used \`persist_record\` (correct frontend-spawn path): **${usedPersist}**`);
  log(`- used \`create_agent\` (predicted mistake for a \`*.ts\` agent): **${usedHostCreate}**`);
  log(`\n## Full tool-call trace`);
  for (const x of trace) {
    log(`${x.n}. \`${x.target}.${x.verb}\`(${x.args})${x.reply ? ` → ${x.reply}` : ""}`);
  }
  log(`\n**Final text:**\n\n> ${String(run.final?.["final"] ?? run.final?.["response"] ?? "(none)").replace(/\n/g, "\n> ")}`);

  // ── snapshot the resulting trees ──
  let webTree: unknown = null;
  let hostTree: unknown = null;
  try {
    webTree = await wsCall(h.origin, "web_loader", { type: "load_tree" });
  } catch (e) {
    log(`\n(web_loader load_tree failed: ${(e as Error).message})`);
  }
  try {
    hostTree = await wsCall(h.origin, "fs_loader", { type: "reflect", tree: "ids", bundles: "none" });
  } catch {
    /* ignore */
  }
  const webRecords = ((webTree as { records?: { id?: string; handler_module?: string }[] })?.records ?? []).filter(
    (r) => r.handler_module && r.handler_module !== "fs_loader.tools",
  );
  log(`\n## Resulting frontend tree (.fantastic/web)`);
  log(webRecords.length ? webRecords.map((r) => `- \`${r.id}\` [${r.handler_module}]`).join("\n") : "- (empty — no frontend agents created)");
  log(`\nHost tree ids: ${JSON.stringify((hostTree as { tree?: string[] })?.tree ?? [])}`);

  // ── browser check: render + click + PROVE the live update (poll for a value) ──
  let buttonClicked = false;
  let liveValue: string | null = null;
  let litPanels = 0;
  const NUM = "(() => { const t=document.body.innerText||''; return /[0-9]{2,}/.test(t) ? t.trim().slice(0,60) : null; })()";
  try {
    await browser.goto(`${h.httpOrigin}/ts_dist/file/_test_canvas.html`);
    const framed = await browser.evalInAnyIframe<boolean>(
      "(() => document.querySelector('button') ? true : null)()",
      12000,
    );
    if (framed) {
      buttonClicked = await browser.clickInAnyIframe("button", 5000);
      // POLL until a numeric value appears in a panel (button → exec → live update)
      liveValue = await browser.evalInAnyIframe<string>(NUM, 12000);
      // how many panels now show a number? 1 = panel1 only (frontend→frontend
      // emit gap); 2 = panel1 AND panel2 (the emit relayed).
      litPanels = (await browser.evalAllIframes<string>(NUM, 1000)).length;
    }
    log(`\n## Browser check`);
    log(`- a panel with a button rendered: **${framed === true}**`);
    log(`- the button was clickable: **${buttonClicked}**`);
    log(`- a live value appeared after click (button → python_runtime → update): **${liveValue !== null}** — \`${liveValue ?? ""}\``);
    log(
      `- panels showing the value: **${litPanels}** ${
        litPanels >= 2
          ? "(panel1 → panel2 emit RELAYED)"
          : "(only panel1 — frontend→frontend emit gap remains)"
      }`,
    );
    log(`- page errors: ${browser.pageErrors.length ? browser.pageErrors.join(" | ").slice(0, 300) : "none"}`);
  } catch (e) {
    log(`\n## Browser check\n- failed: ${(e as Error).message}`);
  }

  const report = lines.join("\n");
  writeFileSync(REPORT_FILE, report);
  // eslint-disable-next-line no-console
  console.log(`\n${"=".repeat(70)}\n${report}\n${"=".repeat(70)}\n(report written to ${REPORT_FILE})`);

  // diagnostic, not a gate: only assert the harness completed a round-trip.
  assert.ok(run.final !== null || run.events.length > 0, "the LLM produced some output (else cloud/auth issue)");
});
