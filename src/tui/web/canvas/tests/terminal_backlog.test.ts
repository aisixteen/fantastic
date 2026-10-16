import { test } from "node:test";
import assert from "node:assert/strict";
import { clearBacklog } from "../src/bundles/terminal_view/backlog.ts";

// A RE-ATTACH STARTS THE COUNT OVER: the exec agent pauses a PTY past 100 000
// unacked chars, and what an earlier page drew but never acked stays counted —
// reload after reload, until the job paused for good. The ring replay shows the
// new page everything so far, so the attach acks what is left counted.

type Call = { target: string; payload: { type: "ack"; job_id: number; chars: number } };

const fake = (unacked: unknown) => {
  const calls: Call[] = [];
  let left = typeof unacked === "number" ? unacked : 0;
  const call = async (target: string, payload: Call["payload"]) => {
    calls.push({ target, payload });
    if (payload["type"] !== "ack") return {};
    left = Math.max(0, left - payload.chars);
    return typeof unacked === "number" ? { unacked: left, paused: left > 100_000 } : unacked;
  };
  return { calls, call, left: () => left };
};

test("an attach acks what the job still counts unacked, reading the count first", async () => {
  const f = fake(95_101);
  await clearBacklog(f.call, "exec_1", 7);
  assert.deepEqual(f.calls, [
    { target: "exec_1", payload: { type: "ack", job_id: 7, chars: 0 } },
    { target: "exec_1", payload: { type: "ack", job_id: 7, chars: 95_101 } },
  ]);
  assert.equal(f.left(), 0);
});

test("nothing counted: only the read", async () => {
  const f = fake(0);
  await clearBacklog(f.call, "exec_1", 7);
  assert.equal(f.calls.length, 1);
});

test("a refused or odd reply acks nothing more", async () => {
  for (const reply of [{ error: "no such job" }, null, { unacked: "x" }, { unacked: -3 }]) {
    const f = fake(reply);
    await clearBacklog(f.call, "exec_1", 7);
    assert.equal(f.calls.length, 1);
  }
});
