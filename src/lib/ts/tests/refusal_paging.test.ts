// #27 one refusal shape, #1 the paging primitive.

import { test } from "node:test";
import assert from "node:assert/strict";
import { CODES, bad, missing, refusal, unknown } from "../src/kernel/refusal.ts";
import { page } from "../src/kernel/paging.ts";

test("every refusal carries a machine field from the CLOSED code set", () => {
  const r = refusal("unauthorized", "nope", { hint: "ask nicely", see: "readme" });
  assert.equal(r["error"], "nope");
  assert.equal(r["reason"], "unauthorized");
  assert.equal(r["hint"], "ask nicely");
  assert.equal(r["see"], "readme");
  // An unknown code is a PROGRAMMING error: the point of a closed set is that
  // it cannot quietly grow at a call site.
  assert.throws(() => refusal("made_up", "x"), /unknown refusal code/);
  assert.equal(CODES.has("bad_arg"), true);
});

test("optional fields are OMITTED, not emitted empty", () => {
  const r = refusal("locked", "no");
  assert.ok(!("hint" in r), "an empty hint was emitted as a key");
  assert.ok(!("see" in r));
});

test("missing/bad name the ARG, and `bad` says what was wanted", () => {
  const m = missing("create_agent", "handler_module");
  assert.equal(m["reason"], "missing_arg");
  assert.equal(m["arg"], "handler_module");
  assert.equal(m["error"], "create_agent: handler_module required");

  const b = bad("send", "timeout", "a positive number of seconds");
  assert.equal(b["reason"], "bad_arg");
  assert.match(String(b["error"]), /must be a positive number of seconds/);
});

test("unknown_agent NAMES the live ids — a bare refusal teaches nothing", () => {
  // Measured 2026-08-04: a model copied the literal placeholder `<id>` from a
  // prompt example, got `no agent '<id>'` 34 times in one run, never corrected.
  const u = unknown("<id>", ["beta", "alpha"]);
  assert.equal(u["reason"], "unknown_agent");
  assert.deepEqual(u["agents"], ["alpha", "beta"]);
});

test("absent limit ⇒ everything, and next is null", () => {
  const items = ["a", "b", "c"];
  const p = page(items, {}, (x) => x);
  assert.deepEqual(p.page, items);
  assert.equal(p.next, null);
});

test("the cursor is the last item's IDENTITY, not an offset", () => {
  const items = ["a", "b", "c", "d", "e"];
  const first = page(items, { limit: 2 }, (x) => x);
  assert.deepEqual(first.page, ["a", "b"]);
  assert.equal(first.next, "b", "the cursor leaked an offset instead of an identity");

  const second = page(items, { limit: 2, after: "b" }, (x) => x);
  assert.deepEqual(second.page, ["c", "d"]);
  assert.equal(second.next, "d");

  const last = page(items, { limit: 2, after: "d" }, (x) => x);
  assert.deepEqual(last.page, ["e"]);
  assert.equal(last.next, null, "a final page must not advertise a next cursor");
});

test("a cursor naming something GONE returns the tail, not the whole list", () => {
  // Restarting from the top would look like a successful walk and silently
  // re-deliver everything the caller already saw.
  const p = page(["a", "b"], { limit: 1, after: "vanished" }, (x) => x);
  assert.deepEqual(p.page, []);
  assert.equal(p.next, null);
});

test("a non-positive or non-integer limit means UNPAGED, never empty", () => {
  for (const limit of [0, -1, 1.5, "2" as unknown as number]) {
    const p = page(["a", "b"], { limit } as never, (x) => x);
    assert.deepEqual(p.page, ["a", "b"], `limit ${String(limit)} truncated the reply`);
  }
});
