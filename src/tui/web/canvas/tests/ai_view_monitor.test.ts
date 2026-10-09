import { test } from "node:test";
import assert from "node:assert/strict";
import { monitorOf, monitorTag } from "../src/bundles/ai_view/ai_view.ts";

// an exec_bridge monitor's delivery, as the AI agent stores it (the lib's E100 shape)
const DELIVERY = '[from runner]\n[monitor 787 · job 514 · from your call 783 · "w5" · job ended]\nFATAL w5: loss diverged (code 9d102301)\n';

test("a monitor delivery parses after its [from] header: id, job, who, label, ended, the lines", () => {
  const d = monitorOf(DELIVERY);
  assert.ok(d);
  assert.deepEqual(d, { id: "787", job: "514", who: "your", label: "w5", ended: true, lines: "FATAL w5: loss diverged (code 9d102301)" });
  assert.equal(monitorTag(d), 'monitor 787 · job 514 "w5"');
});

test("optional parts: no label, not ended, another caller's job, a dropped-lines tail", () => {
  const d = monitorOf("[from runner · msg 12]\n[monitor 9 · job 3 · from planner's call 7]\nline a\nline b\n(+40 earlier lines dropped)");
  assert.ok(d);
  assert.equal(d.label, null);
  assert.equal(d.ended, false);
  assert.equal(d.who, "planner's");
  assert.equal(d.lines, "line a\nline b\n(+40 earlier lines dropped)");
  assert.equal(monitorTag(d), "monitor 9 · job 3");
});

test("not a monitor delivery: a plain [from] message, a reply, a human's turn", () => {
  assert.equal(monitorOf("[from runner]\nhello"), null);
  assert.equal(monitorOf("[reply to 4 · runner start]\n{}"), null);
  assert.equal(monitorOf("[monitor 1 · job 2 · from your call 3]\nno from header"), null);
});
