import { test } from "node:test";
import assert from "node:assert/strict";
import { PROVIDERS, autoName, hostOf, nameOk, providerOf, refFor, savedKeyFor } from "../src/bundles/setup_view/wizard.ts";

test("the four providers, in page 1's order, each with its URL filled", () => {
  assert.deepEqual(PROVIDERS.map((p) => p.label), ["responses", "anthropic", "ollama", "vllm"]);
  assert.ok(PROVIDERS.every((p) => /^https?:\/\//.test(p.endpoint)));
  assert.equal(providerOf("vllm_backend.tools"), "vllm");
});

test("a new agent's auto name: the first free <provider>-<n>", () => {
  assert.equal(autoName("vllm_backend.tools", []), "vllm-1");
  assert.equal(autoName("vllm_backend.tools", ["vllm-1", "vllm-2", "ollama-1"]), "vllm-3");
  assert.ok(nameOk("vllm-3") && !nameOk("my agent") && !nameOk(""));
});

test("a stored key fits only its provider at its host (kern/keys.rs ref_for)", () => {
  assert.equal(hostOf("https://API.openai.com/v1/responses"), "api.openai.com");
  assert.equal(hostOf("http://127.0.0.1:8000/v1"), "127.0.0.1:8000");
  assert.equal(refFor("responses_backend.tools", "https://api.openai.com/v1/responses"), "responses/api.openai.com");
  const keys = [{ ref: "responses/api.openai.com" }];
  assert.equal(savedKeyFor(keys, "responses_backend.tools", "https://api.openai.com/v1/responses"), "responses/api.openai.com");
  assert.equal(savedKeyFor(keys, "responses_backend.tools", "https://my.proxy/v1"), null, "another host: no fit");
  assert.equal(savedKeyFor(keys, "anthropic_backend.tools", "https://api.openai.com/v1"), null, "another provider: no fit");
});
