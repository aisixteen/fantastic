import { test } from "node:test";
import assert from "node:assert/strict";
import { AgentIndex } from "../src/bundles/files/agent_index.ts";
import { IMAGE_MAX, PDF_MAX, TEXT_MAX, agentTarget, editable, modeFor, refine, targetOf, why } from "../src/bundles/preview/preview_model.ts";

const file = (name: string, size = 10) => ({ type: "file" as const, name, size });

test("modeFor: a folder is dir; images and pdf by size; binaries by extension; text by size", () => {
  assert.equal(modeFor({ type: "dir", name: "src" }), "dir");
  assert.equal(modeFor(file("a.PNG")), "image");
  assert.equal(modeFor(file("a.svg", IMAGE_MAX)), "image");
  assert.equal(modeFor(file("a.png", IMAGE_MAX + 1)), "binary");
  assert.equal(modeFor(file("a.pdf", PDF_MAX)), "pdf");
  assert.equal(modeFor(file("a.pdf", PDF_MAX + 1)), "binary");
  for (const n of ["a.zip", "x.wasm", "lib.so", "f.woff2", "m.mp4", "db.sqlite"]) assert.equal(modeFor(file(n)), "binary", n);
  assert.equal(modeFor(file("a.rs", TEXT_MAX)), "text", "exactly 1 MiB is still text");
  assert.equal(modeFor(file("a.rs", TEXT_MAX + 1)), "large", "one byte over is paged");
  assert.equal(modeFor(file("a.md")), "markdown");
  assert.equal(modeFor(file("a.markdown", TEXT_MAX + 1)), "large");
  assert.equal(modeFor(file("Makefile")), "text");
  assert.equal(modeFor(file(".gitignore")), "text", "a dotfile has no extension");
  assert.equal(modeFor(file("a.txt", -1)), "text", "an unknown size reads as small");
});

test("refine: the bridge's bad_arg on path means binary; any other refusal is an error, verbatim", () => {
  assert.deepEqual(refine("text", { path: "a", lines: [] }), { mode: "text" });
  assert.deepEqual(refine("text", { error: "read: path must be a text file", reason: "bad_arg", arg: "path" }), { mode: "binary" });
  assert.deepEqual(refine("markdown", { error: "x", reason: "bad_arg", arg: "path" }), { mode: "binary" });
  assert.deepEqual(refine("text", { error: "file 'a' not found", reason: "not_found" }), { mode: "error", error: "file 'a' not found" });
  assert.deepEqual(refine("text", { error: "denied", reason: "bad_arg", arg: "offset" }), { mode: "error", error: "denied" });
});

test("editable and why", () => {
  assert.equal(editable("text"), true);
  assert.equal(editable("markdown"), true);
  for (const m of ["binary", "large", "image", "pdf", "dir", "error"] as const) {
    assert.equal(editable(m), false, m);
    assert.notEqual(why(m), "", m);
  }
  assert.equal(why("text"), "");
  assert.match(why("large"), /1 MiB/);
  assert.match(why("text", true), /agent/);
});

test("targetOf: follow takes the selection, pinned ignores it", () => {
  assert.equal(targetOf({ follow: true }, "a/b"), "a/b");
  assert.equal(targetOf({ follow: true }, null), null);
  assert.equal(targetOf({ pinned: "x" }, "a/b"), "x");
});

test("agentTarget: an agent folder shows its agent.yaml in file mode, the agent in agent mode", () => {
  const ix = new AgentIndex();
  ix.set(".fantastic/agents/user/agents/files", { id: "files", handler: "file_bridge.tools", displayName: "", hasParent: true, hasMemory: false });
  const p = ".fantastic/agents/user/agents/files";
  assert.deepEqual(agentTarget(p, "file", ix), { path: `${p}/agent.yaml`, agent: "files" });
  assert.deepEqual(agentTarget(p, "agent", ix), { path: p, agent: "files" });
  assert.deepEqual(agentTarget("src", "file", ix), { path: "src", agent: null });
});
