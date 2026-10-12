import { test } from "node:test";
import assert from "node:assert/strict";
import { attached, launched, ran, runLine, savedCmd, shortCmd } from "../src/bundles/terminal_view/last_cmd.ts";

test("last_cmd: the record's saved command, else none", () => {
  assert.equal(savedCmd({ last_cmd: " npm run dev " }), "npm run dev");
  assert.equal(savedCmd({ last_cmd: "" }), null);
  assert.equal(savedCmd({}), null);
  assert.equal(savedCmd(undefined), null);
});

test("last_cmd: run is offered only on a NEW shell with a saved command", () => {
  const s = { cmd: "npm run dev", offer: false };
  assert.deepEqual(attached(s, true, true), { cmd: "npm run dev", offer: true }, "after a restart");
  assert.equal(attached(s, false, true).offer, false, "a live shell joined: nothing to re-run");
  assert.equal(attached(s, true, false).offer, false, "a coding agent's cell runs its own program");
  assert.equal(attached({ cmd: null, offer: false }, true, true).offer, false, "first ever: nothing ran");
});

test("last_cmd: another command running, or run pressed, takes the offer away", () => {
  const offered = { cmd: "npm run dev", offer: true };
  assert.deepEqual(launched(offered, "vim a.txt"), { cmd: "vim a.txt", offer: false });
  assert.deepEqual(ran(offered), { cmd: "npm run dev", offer: false });
  assert.equal(runLine("npm run dev"), "npm run dev\r");
});

test("last_cmd: shortened for the bar, its start kept", () => {
  assert.equal(shortCmd("ls  -la\n"), "ls -la");
  const long = "x".repeat(80);
  assert.equal(shortCmd(long, 10), "xxxxxxxxx…");
});
