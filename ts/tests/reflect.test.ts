import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";

interface AgentSpec {
  id: string;
  parentId: string | null;
  root?: boolean;
  handlerModule?: string;
  sentence?: string;
  verbs?: { [k: string]: string };
  meta?: Payload;
}
interface Case {
  name: string;
  target: string;
  payload: Payload;
  expect: Json;
}
interface Fixture {
  bundles: string[];
  tree: AgentSpec[];
  cases: Case[];
}

const fixtureUrl = new URL(
  "./fixtures/reflect_conformance.json",
  import.meta.url,
);

function buildKernel(fx: Fixture): Kernel {
  const k = new Kernel();
  for (const hm of fx.bundles) k.registerBundle(hm, () => null);
  for (const spec of fx.tree) {
    const agent = new Agent({
      id: spec.id,
      parentId: spec.parentId,
      handlerModule: spec.handlerModule ?? null,
      sentence: spec.sentence ?? null,
      verbs: spec.verbs ?? null,
      meta: spec.meta ?? {},
    });
    k.register(agent);
    if (spec.root) k.setRoot(agent);
  }
  return k;
}

test("reflect matches the cross-language conformance fixture", async () => {
  const fx = JSON.parse(await readFile(fixtureUrl, "utf8")) as Fixture;
  const k = buildKernel(fx);
  for (const c of fx.cases) {
    const got = await k.send(c.target, c.payload);
    assert.deepEqual(got, c.expect, `case: ${c.name}`);
  }
});
