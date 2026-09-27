// A LIVE chain through a real model. Opt-in, and off by default.
//
// The rest of the AI tests fake the provider, which is the right default: unit
// tests must not need a network, a key, or somebody's GPU. But a chain that
// passes against a scripted mind can still fail against a real one — a model
// writes envelopes a fixture never thought of, and only a real turn finds that.
//
// Enable it deliberately (nothing here guesses an endpoint or a model):
//
//   AI_LIVE=1 \
//   AI_ENDPOINT=<an OpenAI-compatible /v1 base> \
//   AI_MODEL=<your model> \
//   AI_KEY=<key, if the endpoint wants one> \
//   node --test tests/ai_live.test.ts
//
// Skipped otherwise, so `npm test` stays hermetic and free.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { registerRamStore } from "../src/bundles/memory/ram_store.ts";
import { registerAIBackend, type AIRecord } from "../src/bundles/ai/ai_backend.ts";
import { settle } from "./ai_world.ts";

const live = process.env["AI_LIVE"] === "1";

test(
  "a real model discovers an agent it was not told about, and uses it",
  { skip: live ? false : "set AI_LIVE=1 to run" },
  async () => {
    const endpoint = process.env["AI_ENDPOINT"];
    const model = process.env["AI_MODEL"];
    if (!endpoint || !model) throw new Error("AI_LIVE=1 needs AI_ENDPOINT and AI_MODEL — nothing is guessed");
    const store: Record<string, Json> = {};
    const k = new Kernel();
    registerRamStore(k);
    registerAIBackend(k, "ai.ts", (id) => k.get(id)!.meta as AIRecord);
    k.registerBundle("store.tools", (_id, payload) => {
      const p = payload as Payload;
      if (p["type"] === "set") {
        store[String(p["key"])] = p["value"] ?? null;
        return { ok: true };
      }
      if (p["type"] === "read") return { key: p["key"] ?? null, value: store[String(p["key"])] ?? null };
      return { error: "unknown verb — reflect me" };
    });
    k.setBundleReadme("store.tools", "A key/value store. Verbs: read {key}, set {key, value}.");
    k.setRoot(new Agent({ id: "kernel_state", meta: { guid: "live" } }));
    k.register(new Agent({ id: "mem", parentId: "kernel_state", handlerModule: "ram_store.ts", meta: { operator: true, admins: ["kernel"], callable_by: { ai: "*" } } }));
    k.register(new Agent({ id: "store", parentId: "kernel_state", handlerModule: "store.tools", meta: { callable_by: { ai: "*" } } }));
    const rec: Payload = { memory_store_id: "mem", can_call: { mem: "*", store: "*" }, endpoint, model, context_window: 32768 };
    if (process.env["AI_KEY"]) rec["api_key"] = process.env["AI_KEY"];
    k.register(new Agent({ id: "ai", parentId: "kernel_state", handlerModule: "ai.ts", meta: rec }));

    // The VERBS are never named here: the model has to reflect the store to learn
    // them. A task that spelled out `<type>set</type>` would test the fixture, not
    // the substrate's self-description.
    await k.send([{
      target: "ai",
      payload: {
        type: "send",
        text:
          "There is an agent called `store` in this kernel. Store the number 42 under the key " +
          "'answer' in it, then read it back to confirm, then end with done.",
      },
    }]);
    await settle(k, 600_000);

    // The PROOF is the store's contents, not the model's prose: a model that says
    // it stored the value without calling anything cannot fake this.
    assert.equal(store["answer"], 42, "the store never got the value");
  },
);
