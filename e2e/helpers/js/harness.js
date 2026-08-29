// Shared scaffolding for the JS-kernel probes.
//
// The python probes drive the python kernel in process; these do the same for
// the browser kernel, running it under node. Same scenarios, same scoring
// shape, same verdict marker — so a result from `agentic_js` is comparable with
// the one from `agentic`, which is the entire point of running them twice.
//
// The model plug is the OpenAI-compatible route, because that is the wire the
// browser backend speaks. `AI_ENDPOINT` is required and never guessed.

import { Kernel } from "../../../src/lib/ts/src/kernel/kernel.ts";
import { Agent } from "../../../src/lib/ts/src/kernel/agent.ts";
import {
  OpenAIProvider,
  runLoop,
  systemPrompt,
} from "../../../src/lib/ts/src/bundles/ai/ai_backend.ts";

export const MARKER = "E2E-VERDICT:";

/** One canonical verdict line, so the launcher parses a token not a dialect. */
export function verdict(ok, note = "") {
  console.log(`\n${MARKER} ${ok ? "PASS" : "FAIL"}${note ? " — " + note : ""}`);
  return ok;
}

export function record() {
  const endpoint = process.env.AI_ENDPOINT;
  if (!endpoint) {
    // The substrate refuses to guess a host to send a key to, and so does this.
    console.error("AI_ENDPOINT is unset — name the OpenAI-compatible route.");
    process.exit(2);
  }
  return { endpoint, model: process.env.AI_MODEL, api_key: process.env.AI_KEY };
}

/** A kernel with a root, ready for a probe to register its fixture into. */
export function kernel() {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  return k;
}

/**
 * Register a fixture agent.
 *
 * `sentence` and `verbs` go on the RECORD, not in a reflect handler: in this
 * kernel `reflect` is kernel-owned and reads the record, where python/rust/swift
 * answer it from the bundle. Leaving them off is not cosmetic — the agent then
 * reflects as "Bare agent … answers substrate verbs only", the model cannot
 * discover what it does, and it guesses verbs that do not exist. Measured here:
 * a probe whose compute agent had no declared verbs got `{"type":"sha256"}`
 * invented at it, twice.
 */
export function register(k, id, handlerModule, handler, { sentence, verbs } = {}) {
  k.registerBundle(handlerModule, handler);
  k.register(
    new Agent({
      id,
      parentId: "kernel_state",
      handlerModule,
      sentence: sentence ?? null,
      verbs: verbs ?? null,
    }),
  );
}

/** Drive the model, printing each turn the way the python probes do. */
export async function drive(k, task, { maxPasses = 12, banner = "" } = {}) {
  const rec = record();
  console.log("=".repeat(74));
  console.log(`  js kernel  ·  ${rec.model ?? "(server default)"}  ·  ${banner}`);
  console.log("=".repeat(74));
  const hist = [
    { role: "system", content: systemPrompt("ai") },
    { role: "user", content: task },
  ];
  let turn = 0;
  const out = await runLoop("ai", k, new OpenAIProvider(rec), hist, {
    maxPasses,
  });
  for (const m of hist) {
    if (m.role === "system") continue;
    turn++;
    const head = `-- ${m.role.toUpperCase()} ${turn} ->`;
    console.log(`\n${head}\n${m.content.slice(0, 1200)}`);
  }
  console.log(`\n  passes: ${out.passes}   calls: ${out.calls}`);
  return out;
}
