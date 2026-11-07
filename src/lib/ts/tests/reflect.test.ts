import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { sendAs } from "./kit.ts";

interface AgentSpec {
  id: string;
  parentId: string | null;
  root?: boolean;
  handlerModule?: string;
  sentence?: string;
  meta?: Payload;
}
interface Case {
  name: string;
  /** reflect AS this agent (gated like any send);
   *  absent = the operator */
  from?: string;
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
      meta: structuredClone(spec.meta ?? {}),
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
    const items = [{ target: c.target, payload: c.payload }];
    const got = (await (c.from !== undefined ? sendAs(k, c.from, items) : k.send(items)))[0]!.result;
    assert.deepEqual(got, c.expect, `case: ${c.name}`);
  }
});

test("a page agent's view rides its reflect; no view, no key", async () => {
  const k = new Kernel();
  const root = new Agent({ id: "kernel", parentId: null, handlerModule: null, sentence: null, meta: {} });
  k.register(root);
  k.setRoot(root);
  const view = { kind: "html", bytes: "<b>hi</b>" };
  for (const [id, meta] of [["drawn", { view }], ["plain", {}]] as const) {
    k.register(new Agent({ id, parentId: "kernel", handlerModule: null, sentence: null, meta: structuredClone(meta) as Payload }));
  }
  const ask = async (id: string) =>
    (await k.send([{ target: id, payload: { type: "reflect" } }]))[0]!.result as Payload;
  assert.deepEqual((await ask("drawn"))["view"], view);
  assert.equal("view" in (await ask("plain")), false);
});
