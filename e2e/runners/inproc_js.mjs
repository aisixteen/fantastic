// Run a SHARED scenario against the BROWSER kernel, in process under node.
//
// Same scenario file as `runners/host.py` — task,
// roster by role, grants, assertions. What differs is only how a role is
// played: this kernel registers a handler in process, so `memory` is a live
// JS handler rather than the `yaml_state` bundle a compiled kernel must use.
//
// One difference that is NOT cosmetic: in this kernel `reflect` is kernel-owned
// and reads `sentence` / `verbs` off the RECORD, where python/rust/swift answer
// it from the bundle. An agent without them reflects as "Bare agent — answers
// substrate verbs only" and the model cannot discover it, so the scenario's
// declared verbs are put on the record here.

import * as nodeCrypto from "node:crypto";
import { createHash, randomBytes } from "node:crypto";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { Kernel } from "../../src/lib/ts/src/kernel/kernel.ts";
import { Agent } from "../../src/lib/ts/src/bundles/../kernel/agent.ts";
import {
  OpenAIProvider,
  runLoop,
  systemPrompt,
} from "../../src/lib/ts/src/bundles/ai/ai_backend.ts";

const E2E = dirname(dirname(fileURLToPath(import.meta.url)));
const MARKER = "E2E-VERDICT:";

/** Minimal YAML reader for the scenario shape — no dependency added. */
function loadScenario(id) {
  // Test dirs carry a stable `E<nn>_<name>` prefix so the suite has an ordered,
  // citable index; the id in the yaml stays bare, so renumbering a directory
  // never rewrites the scenario or the results that cite it.
  const root = join(E2E, "tests", "scenario");
  const dir = readdirSync(root).find((d) => d.split(/_(.*)/s)[1] === id);
  if (!dir) {
    console.error(`no scenario ${id} in ${root}`);
    process.exit(2);
  }
  const text = readFileSync(join(root, dir, "scenario.yaml"), "utf8");
  const sc = { roster: [], grants: {}, expect: [], limits: {} };
  let block = null;
  let entry = null;
  let inVerbs = false;
  for (const raw of text.split("\n")) {
    const line = raw.replace(/\s+$/, "");
    if (!line || line.trimStart().startsWith("#")) continue;
    let m;
    if ((m = line.match(/^(\w+):\s*(.*)$/))) {
      block = m[1];
      inVerbs = false;
      const v = m[2].trim();
      if (v && v !== ">-" && v !== "|") sc[block] = v.replace(/^["']|["']$/g, "");
      continue;
    }
    if (block === "task" && line.startsWith("  ")) {
      sc.task = (sc.task && typeof sc.task === "string" ? sc.task + " " : "") + line.trim();
      continue;
    }
    if (block === "limits" && (m = line.match(/^\s+(\w+):\s*(\d+)/))) {
      sc.limits[m[1]] = Number(m[2]);
      continue;
    }
    if (block === "roster") {
      if ((m = line.match(/^\s+-\s+role:\s*(\w+)/))) {
        entry = { role: m[1], verbs: {} };
        sc.roster.push(entry);
        inVerbs = false;
      } else if (entry && (m = line.match(/^\s+(id|sentence):\s*(.+)$/))) {
        entry[m[1]] = m[2].trim();
      } else if (entry && line.match(/^\s+verbs:\s*$/)) {
        inVerbs = true;
      } else if (entry && inVerbs && (m = line.match(/^\s+(\w+):\s*(.+)$/))) {
        entry.verbs[m[1]] = m[2].trim().replace(/^["']|["']$/g, "");
      }
      continue;
    }
    if (block === "grants" && (m = line.match(/^\s+ai_can_call:\s*\[(.*)\]/))) {
      sc.grants.ai_can_call = m[1].split(",").map((s) => s.trim()).filter(Boolean);
      continue;
    }
    if (block === "expect") {
      // `path:` is the field the scenarios carry (memory became FILES with
      // text_file; the old KV `key:` died with durable_memory). The ASSERTION
      // below already reads `check.path` — only this parser still wrote `key`,
      // so every `- read:` line silently failed to match, `expect` stayed empty,
      // and the following `contains:` crashed on `expect[-1]`. `key:` is still
      // accepted so an older scenario parses, and maps to the same field.
      if ((m = line.match(/^\s+-\s+read:\s*\{agent:\s*(\w+),\s*(?:path|key):\s*([\w./-]+)\}/))) {
        sc.expect.push({ agent: m[1], path: m[2] });
      } else if ((m = line.match(/^\s+contains:\s*"?([^"]+?)"?\s*$/))) {
        // A `contains:` with nothing to attach to is a scenario this runner
        // cannot express. Say so; do not crash with a TypeError on undefined.
        if (sc.expect.length === 0) {
          throw new Error(
            `scenario ${id}: 'contains:' with no preceding '- read:' — the read ` +
            `line did not parse (expected '- read: {agent: <id>, path: <path>}')`,
          );
        }
        sc.expect[sc.expect.length - 1].contains = m[1].trim();
      } else if ((m = line.match(/^\s+-\s+side_effects:\s*\{agent:\s*(\w+),\s*(exactly|at_least):\s*(\d+)\}/))) {
        sc.expect.push({ side_effects: { agent: m[1], [m[2]]: Number(m[3]) } });
      } else if (line.match(/^\s+-\s+human_double_click:/)) {
        sc.expect.push({ human_double_click: true });
      }
    }
  }
  return sc;
}

const id = process.argv[2] ?? "dialog";
const sc = loadScenario(id);

// Per-run placeholders, minted HERE so the expected answer is in no model's
// weights and no cache — the same values the python and host runners mint.
const NONCE = randomBytes(8).toString("hex");
const DIGEST = createHash("sha256").update(NONCE).digest("hex").slice(0, 12);
sc.task = String(sc.task).replaceAll("$NONCE", NONCE).replaceAll("$DIGEST", DIGEST);
for (const e of sc.expect) {
  e.contains = String(e.contains).replaceAll("$NONCE", NONCE).replaceAll("$DIGEST", DIGEST);
}

const stores = {};
// Role state the assertions read — how many times a side effect really ran.
const sideEffects = {};
const k = new Kernel();
k.setRoot(new Agent({ id: "kernel_state" }));

for (const entry of sc.roster) {
  if (entry.role === "render_sink") continue; // no stdout sink needed in process
  if (entry.role === "compute") {
    // The browser kernel has no process edge, so the compute role is an
    // in-process evaluator. `crypto` is NAMED in the reflect verbs: a run
    // surface that silently lacks a stdlib invites the model to hand-roll
    // SHA-256 in one expression, which it did, returning `undefined`.
    k.registerBundle("compute.tools", (_id, p) => {
      if (p.type !== "run") return { error: `unknown verb ${p.type}` };
      try {
        let fn;
        try {
          fn = new Function("crypto", String(p.code));
        } catch {
          fn = new Function("crypto", `return (${p.code})`);
        }
        const v = fn(nodeCrypto);
        return { result: v === undefined ? "undefined" : String(v) };
      } catch (e) {
        return { error: String(e) };
      }
    });
    k.register(
      new Agent({
        id: entry.id,
        parentId: "kernel_state",
        handlerModule: "compute.tools",
        sentence: "Runs JavaScript and returns its value.",
        verbs: {
          run:
            "args: code:str — the BODY of a function; `return` your result. " +
            "node's `crypto` module is in scope as `crypto` " +
            "(e.g. return crypto.createHash('sha256').update(s).digest('hex')).",
        },
      }),
    );
    continue;
  }
  if (entry.role === "dispenser") {
    // Issuing is a SIDE EFFECT: calling twice dispenses twice, which is exactly
    // what the scenario is trying to catch.
    const state = { count: 0, receipts: [] };
    sideEffects[entry.id] = state;
    k.registerBundle("dispenser.tools", (_id, p) => {
      if (p.type === "dispense") {
        state.count += 1;
        const receipt = Math.random().toString(16).slice(2, 10);
        state.receipts.push(receipt);
        return { coin: state.count, receipt, job: p.job };
      }
      if (p.type === "count") return { count: state.count };
      return { error: `unknown verb ${p.type}` };
    });
    k.register(new Agent({
      id: entry.id, parentId: "kernel_state", handlerModule: "dispenser.tools",
      sentence: "Coin dispenser — one coin per `dispense` call. A coin must never be issued twice for one job.",
      verbs: {
        dispense: "args: job:str — issues ONE coin, returns {coin, receipt}. Issuing is a side effect: calling twice dispenses twice.",
        count: "how many coins have ever been dispensed",
      },
    }));
    continue;
  }
  if (entry.role === "flaky_plant") {
    // A pump that fails twice then works, and a valve that never does. Both
    // THROW — a terminal failure is what a retry budget and a dead letter are for.
    const state = { count: 0, receipts: [] };
    sideEffects[entry.id] = state;
    k.registerBundle("plant.tools", (_id, p) => {
      if (p.type === "pump") {
        state.count += 1;
        if (state.count < 3) throw new Error("pump stalled");
        return { pumped: true, serial: Math.random().toString(16).slice(2, 8) };
      }
      if (p.type === "valve") {
        state.count += 1;
        throw new Error("valve is rusted shut");
      }
      return { error: `unknown verb ${p.type}` };
    });
    k.register(new Agent({
      id: entry.id, parentId: "kernel_state", handlerModule: "plant.tools",
      sentence: "Industrial plant. Its pump is unreliable; its valve has seized.",
      verbs: {
        pump: "no args — runs the pump. Fails intermittently.",
        valve: "no args — operates the valve.",
      },
    }));
    continue;
  }
  if (entry.role === "purchase_desk") {
    // Spends real money — only a person may press the button, which is why the
    // AI is granted no edge to it.
    const state = { count: 0, receipts: [] };
    sideEffects[entry.id] = state;
    k.registerBundle("purchase.tools", (_id, p) => {
      if (p.type === "buy") {
        state.count += 1;
        const receipt = Math.random().toString(16).slice(2, 10);
        state.receipts.push(receipt);
        return { bought: p.item, receipt };
      }
      return { error: `unknown verb ${p.type}` };
    });
    k.register(new Agent({
      id: entry.id, parentId: "kernel_state", handlerModule: "purchase.tools",
      sentence: "Purchase desk. Spends real money — only a person may press the button.",
      verbs: { buy: "args: item:str — buys ONE. Spending is a side effect." },
    }));
    continue;
  }
  if (entry.role === "user") {
    // THE HUMAN, as an ordinary agent. Asking used to be a TEXT CONVENTION the
    // harness scraped out of the model's output; here it is a call whose REPLY
    // is the answer, and the task the model receives is a gated send from this
    // agent — so `user` has an id, a record, a reflect entry and a ledger row
    // like any other peer. The answer is POLICY ON THE RECORD, never code.
    const KINDS = ["text", "single", "multi", "confirm"];
    // HOW MANY TIMES THE HUMAN WAS ACTUALLY ASKED. The scorer reads THIS, never
    // the model's prose: a turn that talks about asking leaves the tally at zero.
    // PROPOSALS handed over: a question may carry an `action` the asker may not
    // perform itself. It is HELD, never run here — pressing the button is a
    // separate act by the person, and the runner reads it from a call the human
    // actually received rather than from the model's prose.
    const asks = { n: 0, proposals: [] };
    sideEffects[entry.id] = asks;
    k.registerBundle("user_probe.tools", async (id, p) => {
      if (p.type === "question") {
        const q = p.question;
        if (typeof q !== "string" || !q.trim()) {
          return { error: "user_probe: question: `question` must be a non-empty string",
                   reason: "bad_arg", arg: "question" };
        }
        const kind = p.kind ?? "text";
        if (!KINDS.includes(kind)) {
          return { error: `user_probe: question: \`kind\` must be one of ${KINDS}`,
                   reason: "bad_arg", arg: "kind" };
        }
        const answer = k.get(id)?.answer;
        // NO DEFAULT — an unwired `user` says so rather than inventing a
        // human's words.
        if (typeof answer !== "string" || !answer) {
          return { error: "user_probe: no `answer` on this agent's record — the operator wires what the human says",
                   reason: "not_connected" };
        }
        if (p.action && typeof p.action === "object") asks.proposals.push(p.action);
        asks.n += 1;
        return { answer, request_id: p.request_id ?? null, asks: asks.n };
      }
      if (p.type === "asks") return { asks: asks.n, proposals: asks.proposals };
      if (p.type === "task") {
        if (typeof p.target !== "string" || !p.target) {
          return { error: "user_probe: task: `target` must be a non-empty string",
                   reason: "bad_arg", arg: "target" };
        }
        if (typeof p.text !== "string" || !p.text) {
          return { error: "user_probe: task: `text` must be a non-empty string",
                   reason: "bad_arg", arg: "text" };
        }
        // The send is issued BY this agent, so it is gated and its ledger row
        // names `user` as the sender.
        const out = await k.send([{ target: p.target,
          payload: { type: "send", text: p.text, client_id: p.client_id ?? "user" } }]);
        return { tasked: p.target, result: out[0].result };
      }
      return { error: `user_probe: unknown verb ${p.type}`, reason: "unhandled", verb: p.type };
    });
    k.register(new Agent({
      id: entry.id, parentId: "kernel_state", handlerModule: "user_probe.tools",
      answer: entry.answer,
      sentence: "The human. Ask me when you cannot proceed without a decision that is mine to make; the answer comes straight back as the reply.",
      verbs: {
        question: "args: question:str (req), kind:str? (text|single|multi|confirm), options:list?, action:dict?, request_id:str?. Asks, and RETURNS the answer — {answer, request_id}. An ordinary call: nothing suspends. An `action` is a PROPOSAL the asker may not perform itself: it is HELD, never run here.",
        asks: "no args — {asks, proposals}: how many times this agent has been asked, and every `action` a question handed it. The tally, not prose.",
        task: "args: target:str (req), text:str (req). The human hands work to another agent; the send carries THIS agent as its sender.",
      },
    }));
    continue;
  }
  if (entry.role !== "memory") {
    console.log(`  role ${entry.role} unsupported by this runner`);
    console.log(`\n${MARKER} FAIL — unsupported role ${entry.role}`);
    process.exit(1);
  }
  // Memory is FILES now: an in-process text_file face — documents keyed by
  // path, line-numbered read, append — matching the surface the other
  // runtimes serve with the real bundle. Content is whatever the model wrote.
  const data = {};
  stores[entry.id] = data;
  k.registerBundle(`${entry.id}.tools`, (_id, p) => {
    const body = p.bytes ?? p.bytes_utf8;
    if (p.type === "write") {
      if (!p.path) return { error: "write: path required", reason: "missing_arg" };
      data[p.path] = String(body ?? "");
      return { path: p.path, size: data[p.path].length };
    }
    if (p.type === "append") {
      if (!p.path) return { error: "append: path required", reason: "missing_arg" };
      data[p.path] = (data[p.path] ?? "") + String(body ?? "");
      return { path: p.path, size: data[p.path].length };
    }
    if (p.type === "read") {
      if (!p.path) return { error: "read: path required", reason: "missing_arg" };
      const doc = data[p.path];
      if (doc === undefined)
        return { error: `file '${p.path}' not found`, reason: "not_found", path: p.path };
      const lines = doc.split("\n");
      if (lines.at(-1) === "") lines.pop();
      return {
        path: p.path,
        lines: lines.map((text, i) => ({ n: i + 1, text })),
        total: lines.length,
        eof: true,
      };
    }
    return { error: `unknown verb ${p.type}` };
  });
  // Declared on the RECORD — see the note at the top.
  k.register(
    new Agent({
      id: entry.id,
      parentId: "kernel_state",
      handlerModule: `${entry.id}.tools`,
      sentence: entry.sentence ?? null,
      verbs: entry.verbs ?? null,
    }),
  );
}

const endpoint = process.env.AI_ENDPOINT;
if (!endpoint) {
  console.log("  AI_ENDPOINT is unset — name the OpenAI-compatible route.");
  process.exit(2);
}
console.log("=".repeat(74));
console.log(`  JS kernel  ·  ${process.env.AI_MODEL ?? "(server default)"}  ·  ${sc.title}`);
console.log("=".repeat(74));

const hist = [
  { role: "system", content: systemPrompt("ai") },
  { role: "user", content: sc.task },
];
const out = await runLoop(
  "ai",
  k,
  new OpenAIProvider({ endpoint, model: process.env.AI_MODEL, api_key: process.env.AI_KEY }),
  hist,
  { maxPasses: sc.limits.max_turns ?? 10 },
);
console.log(`\n  passes: ${out.passes}   calls: ${out.calls}`);

let ok = true;
for (const check of sc.expect) {
  if (check.human_double_click) {
    // THE HUMAN. Presses the button twice with the MODEL'S OWN id — an operator
    // send, which is why the AI's lack of a grant does not stop it. The ledger
    // must make the second press harmless.
    // READ THE PROPOSAL OFF THE HUMAN. It used to be regexed out of the model's
    // prose; it is a real call now, so the human HOLDS what it was handed and
    // the runner asks for it — "the person was asked" becomes a fact the kernel
    // witnessed rather than a sentence the model wrote.
    const held = (await k.send([{ target: "user", payload: { type: "asks" } }]))[0].result;
    const props = held?.proposals ?? [];
    const action = props.length ? props[props.length - 1] : null;
    console.log(`\n  proposal: ${JSON.stringify(action)}`);
    if (!action?.target || !action?.id) { ok = false; continue; }
    for (let i = 0; i < 2; i++) {
      await k.send([{ target: action.target, payload: action.payload ?? {}, id: action.id }]);
    }
    continue;
  }
  if (check.side_effects) {
    const q = check.side_effects;
    const n = sideEffects[q.agent]?.count ?? 0;
    const [kind, want] = "exactly" in q ? ["exactly", q.exactly] : ["at_least", q.at_least];
    const good = kind === "exactly" ? n === want : n >= want;
    console.log(`\n  ${q.agent} side effects -> ${n} (${kind} ${want})  ${good ? "OK" : "NO"}`);
    if (!good) ok = false;
    continue;
  }
  // memory is FILES: the scenario's key is the document path; match against
  // the whole document (the model's format is its own).
  const got = stores[check.agent]?.[check.path];
  console.log(`  ${check.agent}.read(${check.path}) -> ${JSON.stringify(got ?? null)}`);
  if (!String(got ?? "").includes(check.contains)) ok = false;
}
console.log(`  RESULT: ${ok ? "PASS — discovered + acted" : "FAIL"}`);
console.log(`\n${MARKER} ${ok ? "PASS" : "FAIL"} — js kernel`);
process.exit(ok ? 0 : 1);
