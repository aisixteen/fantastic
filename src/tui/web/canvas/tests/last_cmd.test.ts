import { test } from "node:test";
import assert from "node:assert/strict";
import { attach, launched, pressed, programName, runLine, savedCmd, shortCmd, stoppedText } from "../src/bundles/terminal_view/last_cmd.ts";

test("last_cmd: the record's saved command, else none", () => {
  assert.equal(savedCmd({ last_cmd: " npm run dev " }), "npm run dev");
  assert.equal(savedCmd({ last_cmd: "" }), null);
  assert.equal(savedCmd({}), null);
  assert.equal(savedCmd(undefined), null);
});

test("nothing starts on its own: only the first placement starts the program; later it waits stopped", () => {
  const s = { cmd: "npm run dev", stopped: false };
  assert.deepEqual(attach(s, false, false), { state: { cmd: "npm run dev", stopped: false }, start: true }, "placed: the human's act starts it");
  assert.deepEqual(attach(s, false, true), { state: { cmd: "npm run dev", stopped: true }, start: false }, "after a restart: stopped, never started for them");
  assert.deepEqual(attach({ cmd: null, stopped: true }, true, true), { state: { cmd: null, stopped: false }, start: false }, "a live job is joined");
});

test("▶ (start or run) ends stopped; a launched command is the last one", () => {
  const off = { cmd: "npm run dev", stopped: true };
  assert.deepEqual(pressed(off), { cmd: "npm run dev", stopped: false });
  assert.deepEqual(launched({ cmd: "npm run dev", stopped: false }, "vim a.txt"), { cmd: "vim a.txt", stopped: false });
  assert.equal(runLine("npm run dev"), "npm run dev\r");
});

test("the stopped overlay names the program: claude, codex, fantastic — or the shell", () => {
  assert.equal(programName(["/Users/me/.local/bin/claude"]), "claude");
  assert.equal(programName(["codex", "--x"]), "codex");
  assert.equal(programName([]), "the shell");
  assert.deepEqual(stoppedText(["/opt/bin/fantastic"]), { title: "fantastic is not running", note: "Nothing starts on its own after a restart — press ▶ to start it." });
});

test("last_cmd: shortened for the bar, its start kept", () => {
  assert.equal(shortCmd("ls  -la\n"), "ls -la");
  assert.equal(shortCmd("x".repeat(80), 10), "xxxxxxxxx…");
});
