// A LIVE agentic loop through a real model. Opt-in, and off by default.
//
// The rest of the AI tests fake the provider, which is the right default: unit
// tests must not need a network, a key, or somebody's GPU. But a loop that
// passes against a scripted provider can still fail against a real one — a
// model writes envelopes a fixture never thought of, and only a real turn
// finds that.
//
// Enable it deliberately:
//
//   AI_LIVE=1 \
//   AI_ENDPOINT=http://127.0.0.1:4000/openai/v1 \
//   AI_MODEL=qwen3.8-27b \
//   AI_KEY=keyless \
//   npm test
//
// Skipped otherwise, so `npm test` stays hermetic and free.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { OpenAIProvider, runLoop, systemPrompt } from "../src/bundles/ai/ai_backend.ts";

const live = process.env.AI_LIVE === "1";

test(
  "a real model discovers an agent it was not told about, and uses it",
  { skip: live ? false : "set AI_LIVE=1 to run" },
  async () => {
    const store: Record<string, unknown> = {};
    const k = new Kernel();
    k.setRoot(new Agent({ id: "kernel_state" }));
    k.registerBundle("store.tools", (_id, payload) => {
      const p = payload as any;
      if (p.type === "reflect") {
        return {
          id: "store",
          sentence: "A key/value store.",
          verbs: { read: "args: key", set: "args: key, value" },
        };
      }
      if (p.type === "set") {
        store[p.key] = p.value;
        return { ok: true };
      }
      if (p.type === "read") return { key: p.key, value: store[p.key] ?? null };
      return { error: "unknown" };
    });
    k.register(
      new Agent({ id: "store", parentId: "kernel_state", handlerModule: "store.tools" }),
    );

    const hist = [
      { role: "system", content: systemPrompt("ai") },
      {
        role: "user",
        // The VERBS are never named here: the model has to reflect the store to
        // learn them. A task that spelled out `{"type":"set"}` would test the
        // fixture, not the substrate's self-description.
        content:
          "There is an agent called `store` in this kernel. Store the number 42 " +
          "under the key 'answer' in it, then read it back to confirm, then say STORED.",
      },
    ];

    const out = await runLoop(
      "ai",
      k,
      new OpenAIProvider({
        endpoint: process.env.AI_ENDPOINT,
        model: process.env.AI_MODEL,
        api_key: process.env.AI_KEY,
      }),
      hist,
      { maxPasses: 8 },
    );

    // The PROOF is the store's contents, not the model's prose: a model that
    // says STORED without calling anything cannot fake this.
    assert.equal(store.answer, 42, `store never got the value; final: ${out.text}`);
    assert.ok(out.calls >= 2, `expected discover+write+read, got ${out.calls} calls`);
  },
);
