// C21 — one semantics, many transports, MANY RUNTIMES.
//
// This driver replays the SHARED corpus (`contract/conformance/cases.json` at the repo
// root) against the ts kernel. Python, rust and swift drivers replay the same
// file. Add a case there and every runtime is on the hook for it — which is the
// whole point: a wire-visible contract is decidable from outside, so asserting
// it once as DATA beats asserting it four times in four languages.
//
// Where this kernel legitimately differs from the host runtimes (it owns no
// tree-CRUD verbs, so `create_agent`/`update_agent`/`list_agents` are not its
// to answer), the case is SKIPPED BY NAME with the reason recorded here rather
// than quietly passing.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { UNHANDLED, refusal } from "../src/kernel/refusal.ts";

const CORPUS = fileURLToPath(
  new URL("../../../../contract/conformance/cases.json", import.meta.url),
);

interface Case {
  name: string;
  target: string;
  payload: Payload;
  expect?: Payload;
  expect_keys?: string[];
  pairwise?: boolean;
  note?: string;
}

const doc = JSON.parse(readFileSync(CORPUS, "utf8")) as { cases: Case[] };

/** This runtime's root id. `$root` in the corpus means exactly this — parity
 *  asserts the per-runtime value, never equality. */
const ROOT_ID = "kernel_state";

/**
 * Cases this kernel does not answer, and WHY. Not a convenience list: each name
 * here is a deliberate, defended difference, and anything not named must pass.
 */
const SKIP: { [name: string]: string } = {
  "kernel-verb-refusal":
    "create_agent is not a verb this kernel owns — the frontend tree is built by " +
    "its bundles and persisted through the host loader. Advertising or answering " +
    "it would be inventing a surface that does not exist.",
  "update-nothing": "same — update_agent belongs to the host root.",
  "update-agent-shape": "same — update_agent belongs to the host root, so this kernel has no reply shape to grade.",
  "delete-unknown": "same — delete_agent belongs to the host root; this kernel deletes nothing.",
  "create-empty-id":
    "same — create_agent belongs to the host root, so an id this kernel never mints " +
    "has no shape to grade. The RULE it pins (a supplied id is validated, never " +
    "replaced by a mint) lives where ids are minted, which is not here.",
  "list-agents-limited":
    "same — list_agents belongs to the host root; `reflect tree=all` is how this " +
    "kernel's own tree is read, and it is paged by the shared `page()` primitive.",
};

function substRoot(v: Json): Json {
  if (v === "$root") return ROOT_ID;
  if (Array.isArray(v)) return v.map(substRoot);
  if (v !== null && typeof v === "object") {
    return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, substRoot(x)]));
  }
  return v;
}

/** Deep subset: the corpus asserts what MUST be there, never the whole reply. */
function subset(expected: Json, actual: Json): boolean {
  if (expected !== null && typeof expected === "object" && !Array.isArray(expected)) {
    if (actual === null || typeof actual !== "object" || Array.isArray(actual)) return false;
    return Object.entries(expected).every(
      ([k, v] ) => k in actual && subset(v, (actual as { [k: string]: Json })[k] as Json),
    );
  }
  return JSON.stringify(expected) === JSON.stringify(actual);
}

function wired(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: ROOT_ID }));
  // The fixture the corpus names: `c21_probe` answers `poke`; `c21_nobody`
  // never exists.
  k.registerBundle("c21_echo.tools", (id, payload) => {
    const verb = typeof payload["type"] === "string" ? payload["type"] : "";
    if (verb === "boot" || verb === "shutdown") return null;
    if (verb === "poke") return { poked: payload["value"] ?? null };
    // Anything else — including an EMPTY payload — is `unhandled`, never a
    // silent null. This mirrors the python probe exactly, because the corpus
    // describes the fixture's behaviour and both must be the same agent.
    return refusal(UNHANDLED, `c21_probe has no verb '${verb}'`, {
      extra: { verb },
    }) as Json;
  });
  k.register(
    new Agent({
      id: "c21_probe",
      parentId: ROOT_ID,
      handlerModule: "c21_echo.tools",
      sentence: "C21 probe — answers `poke` identically on every transport.",
      verbs: { poke: "args: value:any — echoes {poked: value}" },
    }),
  );
  return k;
}

test("the shared C21 corpus is non-empty and every case is accounted for", () => {
  assert.ok(doc.cases.length > 0, "corpus loaded no cases");
  for (const c of doc.cases) {
    assert.ok(c.name && c.target, `malformed case: ${JSON.stringify(c)}`);
  }
});

for (const c of doc.cases) {
  const skip = SKIP[c.name];
  test(`C21 ${c.name}${skip !== undefined ? " (n/a here)" : ""}`, async (t) => {
    if (skip !== undefined) {
      t.diagnostic(skip);
      return;
    }
    const k = wired();
    const [out] = await k.send([{ target: c.target, payload: c.payload }]);
    const reply = out!.result;

    if (c.expect !== undefined) {
      const want = substRoot(c.expect as Json);
      assert.ok(
        subset(want, reply),
        `${c.name}: expected superset of ${JSON.stringify(want)}, got ${JSON.stringify(reply)}`,
      );
    }
    for (const key of c.expect_keys ?? []) {
      assert.ok(
        reply !== null && typeof reply === "object" && key in reply,
        `${c.name}: reply missing key '${key}': ${JSON.stringify(reply)}`,
      );
    }
  });
}
