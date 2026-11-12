import { test } from "node:test";
import assert from "node:assert/strict";
import { check, docOf, docsOf, entry } from "../src/bundles/declared.ts";
import type { Declared } from "../src/bundles/declared.ts";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";

const D: Declared = {
  go: {
    doc: "Goes.",
    args: {
      id: { type: "string", required: true, min: 1, doc: "Who." },
      n: { type: "number", min: 1, default: 5, doc: "How many." },
      mode: { type: "string", enum: ["a", "b"], doc: "How." },
      body: { type: "bytes", doc: "Verbatim." },
      verbs: { type: "string[]", min: 1, doc: "Which." },
      x: { type: "number", doc: "Left." },
      y: { type: "number", doc: "Top." },
      path: { type: "string", doc: "A path." },
      agent: { type: "string", min: 1, doc: "An agent." },
    },
    oneOf: ["path", "agent"],
    together: [["x", "y"]],
    when: [{ arg: "mode", is: "b", then: "body" }],
  },
  none: { doc: "Nothing.", args: {} },
};

const go = (p: Record<string, unknown>) => check("t", "go", D["go"]!, { type: "go", ...p } as Payload);
const refused = (p: Record<string, unknown>): Record<string, unknown> => {
  const r = go(p);
  assert.ok("refusal" in r, JSON.stringify(r));
  return (r as { refusal: Record<string, unknown> }).refusal;
};

test("a call is completed with the declared defaults", () => {
  const r = go({ id: "a", path: "p" });
  assert.ok("args" in r);
  assert.equal((r as { args: Payload }).args["n"], 5);
});

test("a missing required argument, a wrong type and an unknown argument are refused by name", () => {
  assert.deepEqual([refused({ path: "p" })["reason"], refused({ path: "p" })["arg"]], ["missing_arg", "id"]);
  assert.deepEqual([refused({ id: "a", path: "p", n: "5" })["reason"], refused({ id: "a", path: "p", n: "5" })["arg"]], ["bad_arg", "n"]);
  assert.equal(refused({ id: "a", path: "p", n: 0 })["arg"], "n");
  assert.equal(refused({ id: "a", path: "p", mode: "c" })["arg"], "mode");
  assert.equal(refused({ id: "a", path: "p", verbs: [1] })["arg"], "verbs");
  assert.equal(refused({ id: "a", path: "p", verbs: [] })["arg"], "verbs");
  assert.equal(refused({ id: "a", path: "p", zzz: 1 })["arg"], "zzz");
});

test("bytes take a string or the raw bytes of one, nothing else", () => {
  assert.ok("args" in go({ id: "a", path: "p", body: "x" }));
  assert.ok("args" in go({ id: "a", path: "p", body: new TextEncoder().encode("x") }));
  assert.equal(refused({ id: "a", path: "p", body: { a: 1 } })["arg"], "body");
});

test("alternatives, arguments that go together and arguments another one needs", () => {
  assert.deepEqual([refused({ id: "a" })["reason"], refused({ id: "a" })["arg"]], ["missing_arg", "path"]);
  assert.equal(refused({ id: "a", path: "p", agent: "z" })["reason"], "bad_arg");
  assert.equal(refused({ id: "a", path: "p", x: 1 })["arg"], "y");
  assert.ok("args" in go({ id: "a", path: "p", x: 1, y: 2 }));
  assert.deepEqual([refused({ id: "a", path: "p", mode: "b" })["reason"], refused({ id: "a", path: "p", mode: "b" })["arg"]], ["missing_arg", "body"]);
});

test("docs are rendered from the declaration", () => {
  const text = docOf(D["go"]!);
  assert.match(text, /^Goes\. args: id:string, n:number\? = 5, mode:a\|b\?/);
  assert.match(text, /id: Who\./);
  assert.equal(docsOf(D)["none"], "Nothing. No args.");
});

test("entry judges a call before the handler, names what the agent answers, and passes the lifecycle on", async () => {
  const k = new Kernel();
  let seen: Payload | null = null;
  k.registerBundle("t.ts", entry("t", D, (_id, p) => { seen = p; return { ok: true }; }, ["boot"]), docsOf(D));
  k.setRoot(new Agent({ id: "root" }));
  k.register(new Agent({ id: "a", parentId: "root", handlerModule: "t.ts" }));
  const ask = async (p: Record<string, unknown>): Promise<Payload> => (await k.send([{ target: "a", payload: p as Payload }]))[0]!.result as Payload;
  assert.equal((await ask({ type: "go", path: "p" }))["reason"], "missing_arg");
  assert.equal(seen, null, "the handler never ran");
  const unknown = await ask({ type: "nope" });
  assert.equal(unknown["reason"], "unhandled");
  assert.match(String(unknown["error"]), /go, none/);
  assert.deepEqual(await ask({ type: "go", id: "a", path: "p" }), { ok: true });
  assert.equal((seen as unknown as Payload)["n"], 5);
  assert.deepEqual(await ask({ type: "boot" }), { ok: true });
});
