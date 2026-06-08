// The workflow keystone: an AI agent (and the scheduler, and python_runtime) are
// FIRST-CLASS units that drive a real browser UI. Five scenarios, all bypass-free
// (panels reach the host only through the JS kernel), headless Chrome asserting:
//
//   A. scheduler → python_runtime → 3rd html panel        (DETERMINISTIC — always runs)
//   B. scheduler → anthropic ai_agent → 3rd html panel    (live: "apples or bananas")
//   C. AI tool-call → 3rd html panel                       (live: AI actively drives UI)
//   D. AI tool-call → python_runtime                       (live: AI spawns compute)
//   E. AI tool-call → another AI (leaf)                    (live: AI→AI, no recursion)
//
// A proves the scheduler→unit→panel chain with zero LLM. B–E exercise REAL tool-use
// decisions, so they need a reachable Anthropic key — they t.skip cleanly without one
// (the recursion-deadlock guard itself is covered deterministically in the ollama
// backend unit tests, test_tool_call_to_self_does_not_deadlock).
//
// Run: (cd ts && npm run build) then: cd integration_tests/py_ts &&
//      node --test --test-force-exit scheduler_ai_html.browser.itest.ts

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, writeFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import { bootHost, teardownHost, ACTIVE_LLM, llmReachable, LLM_BACKEND, DIST_DIR } from "./_host.ts";
import type { BootOptions, Host } from "./_host.ts";
import { Browser, chromeAvailable } from "./_chrome.ts";

const MOUNT_HTML = `<!doctype html><html><head><meta charset="utf-8">
<title>fantastic · canvas · scheduler/ai test</title>
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

interface Panel {
  id: string;
  html: string;
}

// Seed the frontend store: a canvas root + N html_agent panels under it.
function seedCanvas(tmp: string, panels: Panel[]): void {
  const canvasDir = join(tmp, ".fantastic", "web", "agents", "canvas");
  mkdirSync(canvasDir, { recursive: true });
  writeFileSync(
    join(canvasDir, "agent.json"),
    JSON.stringify({ id: "canvas", handler_module: "canvas.ts", display_name: "canvas" }),
  );
  for (const p of panels) {
    const d = join(canvasDir, "agents", p.id);
    mkdirSync(d, { recursive: true });
    writeFileSync(
      join(d, "agent.json"),
      JSON.stringify({
        id: p.id,
        handler_module: "html_agent.ts",
        display_name: p.id,
        html: p.html,
      }),
    );
  }
}

// Boot a host, seed the tree (with the live ids), open the canvas, run the body.
async function scenario(
  port: number,
  opts: BootOptions,
  seed: (tmp: string, host: Host) => void,
  body: (host: Host, b: Browser) => Promise<void>,
): Promise<void> {
  let host: Host | null = null;
  let browser: Browser | null = null;
  try {
    host = await bootHost(port, opts);
    seed(host.tmp, host);
    writeFileSync(MOUNT_FILE, MOUNT_HTML);
    browser = await Browser.launch();
    await browser.goto(`${host.httpOrigin}/ts_dist/file/_test_canvas.html`);
    await browser.waitFor(
      "!!document.getElementById('canvas') && document.querySelectorAll('.agent-frame').length >= 1",
      20000,
    );
    await body(host, browser);
  } finally {
    if (browser !== null) browser.close();
    if (host !== null) teardownHost(host);
  }
}

// Read #out from whichever subframe has it (panels are null-origin srcdoc iframes).
const READ_OUT =
  "(() => { const e = document.getElementById('out'); return e && e.textContent !== '—' ? e.textContent : null; })()";

// Live-LLM gate + backend selector: `llmReachable()` / `ACTIVE_LLM` resolve to
// Claude (default) or a local ollama model via the `LLM_BACKEND` env switch — so
// this whole A–J suite is the PARITY harness: identical chains, either backend,
// no fork. Offline / missing-key / missing-model skips cleanly (never hangs).
const J = (s: string): string => JSON.stringify(s);
const W = (ms: number): number => Math.round(ms * Number(process.env.LLM_WAIT_X ?? 1)); // scale LLM waits for slow local models

// ─── A. scheduler → python_runtime → 3rd html panel (deterministic) ──

test("A: scheduler → python_runtime → 3rd html panel (deterministic chain)", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  await scenario(
    8920,
    { webLoader: true, serveDist: true, pythonRuntime: true, scheduler: true },
    (tmp, host) => {
      const py = host.pyId as string;
      const sch = host.schedulerId as string;
      const html = `<pre id="out">—</pre><script>
let sid = null;
const out = () => document.getElementById("out");
fantastic.watch(${J(py)}, (ev) => {                       // watch the host compute unit
  if (ev && ev.type === "progress" && ev.stream === "stdout" && /SCHED-OK/.test(ev.line || "")) {
    out().textContent = ev.line;
    if (sid) { fantastic.send(${J(sch)}, { type: "unschedule", schedule_id: sid }); sid = null; } // one-shot
  }
});
fantastic.send(${J(sch)}, { type: "schedule", target: ${J(py)}, interval_seconds: 1,
  payload: { type: "start", code: "print('SCHED-OK-' + str(6*7))" } }).then((r) => { sid = r && r.schedule_id; });
</script>`;
      seedCanvas(tmp, [{ id: "panel3", html }]);
    },
    async (_host, b) => {
      const out = await b.evalInAnyIframe<string>(READ_OUT, 20000);
      assert.ok(out && /SCHED-OK-42/.test(out), `panel shows the scheduled python result (got ${JSON.stringify(out)})`);
      assert.deepEqual(b.pageErrors, [], "no uncaught page errors");
    },
  );
});

// ─── B. scheduler → anthropic ai_agent → 3rd html panel (live) ──

test("B: scheduler → anthropic ai_agent → 3rd html panel (apples or bananas)", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  if (!(await llmReachable())) return t.skip(`${LLM_BACKEND} backend unreachable (key/server/model)`);
  await scenario(
    8921,
    { webLoader: true, serveDist: true, scheduler: true, llm: ACTIVE_LLM },
    (tmp, host) => {
      const ai = host.ollamaId as string;
      const sch = host.schedulerId as string;
      const html = `<pre id="out">—</pre><script>
let sid = null, buf = "";
const out = () => document.getElementById("out");
fantastic.watch(${J(ai)}, (ev) => {                       // watch the AI worker's stream
  if (!ev || ev.client_id !== "sched") return;
  if (ev.type === "token") buf += (ev.text || "");
  if (ev.type === "status" && ev.phase === "done") {
    out().textContent = buf.trim();
    if (sid) { fantastic.send(${J(sch)}, { type: "unschedule", schedule_id: sid }); sid = null; }
  }
});
fantastic.send(${J(sch)}, { type: "schedule", target: ${J(ai)}, interval_seconds: 1,
  payload: { type: "send", client_id: "sched",
    text: "Reply with exactly one word, either apples or bananas. Output only that word." } })
  .then((r) => { sid = r && r.schedule_id; });
</script>`;
      seedCanvas(tmp, [{ id: "panel3", html }]);
    },
    async (_host, b) => {
      const out = await b.evalInAnyIframe<string>(READ_OUT, W(45000));
      assert.ok(out && /\b(apples|bananas)\b/i.test(out), `panel shows the AI's scheduled answer (got ${JSON.stringify(out)})`);
    },
  );
});

// ─── C. AI tool-call → 3rd html panel (AI actively drives the UI) ──

test("C: AI tool-call → 3rd html panel (AI drives the UI by id)", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  if (!(await llmReachable())) return t.skip(`${LLM_BACKEND} backend unreachable (key/server/model)`);
  await scenario(
    8922,
    { webLoader: true, serveDist: true, llm: ACTIVE_LLM },
    (tmp, host) => {
      const ai = host.ollamaId as string;
      // The panel creates a host "bus" agent at runtime, watches it, then asks the
      // AI to push its answer onto the bus via the AI's own send tool. The bus
      // fan-outs the AI's payload to the watching panel (send fan-outs before the
      // handler runs), so the AI's tool-call lands on the UI.
      const html = `<pre id="out">—</pre><script>
const out = () => document.getElementById("out");
(async () => {
  await fantastic.send("fs_loader", { type: "create_agent", handler_module: "yaml_state.tools", id: "bus" });
  fantastic.watch("bus", (ev) => {
    if (ev && ev.type === "set" && ev.key === "display") out().textContent = String(ev.value);
  });
  fantastic.send(${J(ai)}, { type: "send", client_id: "tc",
    text: 'Display your answer on the UI: call the send tool with target_id "bus" and payload ' +
          '{"type":"set","key":"display","value":"<W>"} where <W> is exactly the word apples or bananas. ' +
          'Then reply with that one word.' });
})();
</script>`;
      seedCanvas(tmp, [{ id: "panel3", html }]);
    },
    async (_host, b) => {
      const out = await b.evalInAnyIframe<string>(READ_OUT, W(45000));
      assert.ok(out && /\b(apples|bananas)\b/i.test(out), `AI drove the panel via a tool-call (got ${JSON.stringify(out)})`);
    },
  );
});

// ─── D. AI tool-call → python_runtime (AI spawns compute) ──

test("D: AI tool-call → python_runtime (AI spawns a compute job)", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  if (!(await llmReachable())) return t.skip(`${LLM_BACKEND} backend unreachable (key/server/model)`);
  await scenario(
    8923,
    { webLoader: true, serveDist: true, pythonRuntime: true, llm: ACTIVE_LLM },
    (tmp, host) => {
      const ai = host.ollamaId as string;
      const py = host.pyId as string;
      // The panel watches python_runtime; it asks the AI to run a python job there.
      // The JOB's own progress (not the AI) lands on the panel — proving AI→compute.
      const html = `<pre id="out">—</pre><script>
const out = () => document.getElementById("out");
fantastic.watch(${J(py)}, (ev) => {
  if (ev && ev.type === "progress" && ev.stream === "stdout" && /(^|\\D)42(\\D|$)/.test(ev.line || ""))
    out().textContent = ev.line;
});
fantastic.send(${J(ai)}, { type: "send", client_id: "d",
  text: 'Compute six times seven using the python runtime agent ${py}: call the send tool with ' +
        'target_id "${py}" and payload {"type":"start","code":"print(6*7)"}. Then reply done.' });
</script>`;
      seedCanvas(tmp, [{ id: "panel3", html }]);
    },
    async (_host, b) => {
      const out = await b.evalInAnyIframe<string>(READ_OUT, W(45000));
      assert.ok(out && /42/.test(out), `the AI-spawned python job's result reached the panel (got ${JSON.stringify(out)})`);
    },
  );
});

// ─── E. AI tool-call → another AI (leaf) — AI→AI, no recursion ──

test("E: AI tool-call → another AI leaf (AI→AI, no recursion)", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  if (!(await llmReachable())) return t.skip(`${LLM_BACKEND} backend unreachable (key/server/model)`);
  await scenario(
    8924,
    { webLoader: true, serveDist: true, llm: ACTIVE_LLM },
    (tmp, host) => {
      const ai = host.ollamaId as string;
      // The panel spins up a SECOND, leaf AI (shares the llm_files history agent),
      // watches the driver A's stream, then asks A to delegate to B. B only answers
      // (never calls back) → A→B is a 2-node chain, safe by construction; the
      // _call_stack guard would refuse any cycle before it could deadlock.
      const html = `<pre id="out">—</pre><script>
let buf = "";
const out = () => document.getElementById("out");
(async () => {
  await fantastic.send("fs_loader", { type: "create_agent",
    handler_module: "anthropic_backend.tools", file_agent_id: "llm_files", id: "aileaf" });
  fantastic.watch(${J(ai)}, (ev) => {
    if (!ev || ev.client_id !== "main") return;
    if (ev.type === "token") buf += (ev.text || "");
    if (ev.type === "status" && ev.phase === "done") out().textContent = buf.trim();
  });
  fantastic.send(${J(ai)}, { type: "send", client_id: "main",
    text: 'Delegate to the leaf agent "aileaf": call the send tool with target_id "aileaf" and payload ' +
          '{"type":"send","text":"Reply with exactly one word, either apples or bananas.","client_id":"leaf"}. ' +
          'Then reply with exactly the one word aileaf returned.' });
})();
</script>`;
      seedCanvas(tmp, [{ id: "panel3", html }]);
    },
    async (_host, b) => {
      const out = await b.evalInAnyIframe<string>(READ_OUT, W(60000));
      assert.ok(out && /\b(apples|bananas)\b/i.test(out), `driver A relayed leaf B's answer (got ${JSON.stringify(out)})`);
    },
  );
});

// ─── F. python JOB connector in a live workflow (the keystone gap) ──

test("F: python job uses its kernel connector to call an AI → result on a panel", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  if (!(await llmReachable())) return t.skip(`${LLM_BACKEND} backend unreachable (key/server/model)`);
  await scenario(
    8925,
    { webLoader: true, serveDist: true, pythonRuntime: true, llm: ACTIVE_LLM },
    (tmp, host) => {
      const ai = host.ollamaId as string;
      const py = host.pyId as string;
      // The spawned python JOB reaches BACK into the kernel via its injected
      // connector — it calls the AI itself and prints the answer. Proves PY→AI in a
      // LIVE workflow (not just the connector unit tests): panel → python_runtime →
      // (job's kernel.send → AI) → job stdout → panel watches python_runtime.
      const JOB =
        `r = kernel.send(${J(ai)}, {"type":"send",` +
        `"text":"Reply with exactly one word, apples or bananas. Output only that word.",` +
        `"client_id":"f"})\n` +
        `print("ANS=" + str(r.get("response", "")).strip())`;
      const html = `<pre id="out">—</pre><script>
const out = () => document.getElementById("out");
fantastic.watch(${J(py)}, (ev) => {
  if (ev && ev.type === "progress" && ev.stream === "stdout" && /ANS=/.test(ev.line || "")) out().textContent = ev.line;
});
fantastic.send(${J(py)}, { type: "start", code: ${JSON.stringify(JOB)} });
</script>`;
      seedCanvas(tmp, [{ id: "panel3", html }]);
    },
    async (_host, b) => {
      const out = await b.evalInAnyIframe<string>(READ_OUT, W(50000));
      assert.ok(
        out && /ANS=.*\b(apples|bananas)\b/i.test(out),
        `python job reached the AI via its connector (got ${JSON.stringify(out)})`,
      );
    },
  );
});

// ─── G. ai_view mounts + renders its backend (previously untested) ──

test("G: ai_view renders inline fronting an AI backend", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  await scenario(
    8926,
    { webLoader: true, serveDist: true, llm: ACTIVE_LLM },
    (tmp, host) => {
      const ai = host.ollamaId as string;
      const canvasDir = join(tmp, ".fantastic", "web", "agents", "canvas");
      mkdirSync(join(canvasDir, "agents", "chat"), { recursive: true });
      writeFileSync(
        join(canvasDir, "agent.json"),
        JSON.stringify({ id: "canvas", handler_module: "canvas.ts", display_name: "canvas" }),
      );
      // an ai_view member (chat mode) fronting the host AI backend purely by id
      writeFileSync(
        join(canvasDir, "agents", "chat", "agent.json"),
        JSON.stringify({
          id: "chat",
          handler_module: "ai_view.ts",
          display_name: "AI",
          backend_id: ai,
          mode: "chat",
        }),
      );
    },
    async (_host, b) => {
      // ai_view renders INLINE into the canvas frame body (not an iframe); on mount
      // it reflects the backend and shows the model — no live inference needed.
      await b.waitFor("!!document.querySelector('.chat-view')", 20000);
      const model = await b.evaluate<string>(
        "(() => { const e = document.querySelector('.chat-view .c-model'); return e ? e.textContent : ''; })()",
      );
      // model-agnostic: the rendered backend model must match the ACTIVE backend
      // (e.g. "claude-opus-4-8" or "gemma4:12b") — not a hard-coded "claude".
      const wantModel = (ACTIVE_LLM.model ?? "").split(":")[0];
      assert.ok(
        (model || "").length > 0 && (wantModel === "" || (model || "").includes(wantModel)),
        `ai_view mounted + shows its backend model (got ${JSON.stringify(model)}, want ~${ACTIVE_LLM.model})`,
      );
      assert.deepEqual(b.pageErrors, [], "no uncaught page errors");
    },
  );
});

// ─── H. AI composes a schedule → scheduler → python → panel (closes AI→scheduler) ──

test("H: AI tool-calls scheduler to compose a schedule → it fires python → panel", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  if (!(await llmReachable())) return t.skip(`${LLM_BACKEND} backend unreachable (key/server/model)`);
  await scenario(
    8927,
    { webLoader: true, serveDist: true, pythonRuntime: true, scheduler: true, llm: ACTIVE_LLM },
    (tmp, host) => {
      const ai = host.ollamaId as string;
      const py = host.pyId as string;
      const sched = host.schedulerId as string;
      // The AI itself COMPOSES a schedule (the previously-untested AI→scheduler edge).
      const SP_H =
        "You have ONE tool: send(target_id, payload). Call it EXACTLY once with " +
        `target_id="${sched}" and payload=` +
        `{"type":"schedule","target":"${py}","interval_seconds":2,` +
        `"payload":{"type":"start","code":"print('SCHED-BY-AI')"}}. ` +
        "After the tool result, reply with the single word: scheduled.";
      const drive = `<script>
fantastic.send(${J(ai)}, { type: "send", client_id: "h", system_prompt: ${JSON.stringify(SP_H)}, text: "Schedule it now." });
</script>`;
      const out = `<pre id="out">—</pre><script>
const o = () => document.getElementById("out");
fantastic.watch(${J(py)}, (ev) => {
  if (ev && ev.type === "progress" && ev.stream === "stdout" && /SCHED-BY-AI/.test(ev.line || "")) o().textContent = ev.line;
});
</script>`;
      seedCanvas(tmp, [{ id: "drive", html: drive }, { id: "out", html: out }]);
    },
    async (_host, b) => {
      const out = await b.evalInAnyIframe<string>(READ_OUT, W(60000));
      assert.ok(out && /SCHED-BY-AI/.test(out), `AI-composed schedule fired the python job (got ${JSON.stringify(out)})`);
    },
  );
});

// ─── I. scheduler → python JOB → AI (via connector) → panel (deep cascade) ──

test("I: scheduler fires a python job that calls an AI via its connector → panel", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  if (!(await llmReachable())) return t.skip(`${LLM_BACKEND} backend unreachable (key/server/model)`);
  await scenario(
    8928,
    { webLoader: true, serveDist: true, pythonRuntime: true, scheduler: true, llm: ACTIVE_LLM },
    (tmp, host) => {
      const ai = host.ollamaId as string;
      const py = host.pyId as string;
      const sched = host.schedulerId as string;
      // scheduled job code (TEST-written, so no model-verbatim risk) that itself calls the AI.
      const JOB_I =
        `r = kernel.send(${J(ai)}, {"type":"send",` +
        `"text":"Reply with exactly one word, apples or bananas.","client_id":"i"})\n` +
        `print("DEEP=" + str(r.get("response", "")).strip())`;
      const out = `<pre id="out">—</pre><script>
const o = () => document.getElementById("out");
fantastic.watch(${J(py)}, (ev) => {
  if (ev && ev.type === "progress" && ev.stream === "stdout" && /DEEP=/.test(ev.line || "")) o().textContent = ev.line;
});
fantastic.send(${J(sched)}, { type: "schedule", target: ${J(py)}, interval_seconds: 2,
  payload: { type: "start", code: ${JSON.stringify(JOB_I)} } });
</script>`;
      seedCanvas(tmp, [{ id: "out", html: out }]);
    },
    async (_host, b) => {
      const out = await b.evalInAnyIframe<string>(READ_OUT, W(75000));
      assert.ok(
        out && /DEEP=.*\b(apples|bananas)\b/i.test(out),
        `scheduled python job reached the AI via its connector (got ${JSON.stringify(out)})`,
      );
    },
  );
});

// ─── J. scheduler tick → JS panel (watches the fire) → AI → panel (scheduler-js-ai) ──

test("J: scheduler tick drives a panel that calls the AI", async (t) => {
  if (!chromeAvailable()) return t.skip("system Chrome not found");
  if (!existsSync(join(DIST_DIR, "main.js"))) return t.skip("ts/dist not built");
  if (!(await llmReachable())) return t.skip(`${LLM_BACKEND} backend unreachable (key/server/model)`);
  await scenario(
    8929,
    { webLoader: true, serveDist: true, scheduler: true, llm: ACTIVE_LLM },
    (tmp, host) => {
      const ai = host.ollamaId as string;
      const sched = host.schedulerId as string;
      const out = `<pre id="out">—</pre><script>
const o = () => document.getElementById("out");
let sent = false;
fantastic.watch(${J(sched)}, (ev) => {                       // scheduler emits schedule_fired on its OWN id
  if (ev && ev.type === "schedule_fired" && !sent) {
    sent = true;
    fantastic.send(${J(ai)}, { type: "send", client_id: "j", text: "Reply with exactly one word, apples or bananas." })
      .then((r) => { o().textContent = "JAI=" + String((r && r.response) || "").trim(); });
  }
});
fantastic.send(${J(sched)}, { type: "schedule", target: ${J(sched)}, interval_seconds: 2, payload: { type: "list" } });
</script>`;
      seedCanvas(tmp, [{ id: "out", html: out }]);
    },
    async (_host, b) => {
      const out = await b.evalInAnyIframe<string>(READ_OUT, W(60000));
      assert.ok(
        out && /JAI=.*\b(apples|bananas)\b/i.test(out),
        `scheduler tick drove the panel to call the AI (got ${JSON.stringify(out)})`,
      );
    },
  );
});
