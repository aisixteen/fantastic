import { test } from "node:test";
import assert from "node:assert/strict";
import { plan } from "../src/bundles/home_view/pages.ts";
import { ACTIVITY_EVERY_MS, NO_OVERLAY, activityDue, decodeLock, overlayFor, overlayInput, systemRow, userSocketUrl } from "../src/bundles/home_view/lock_model.ts";
import type { HomeRow, Status } from "../src/bundles/home_view/home_model.ts";

function row(id: string, status: Status, port = 0): HomeRow {
  return {
    id, guid: id, name: id, path: `/f/${id}`, last_seen: 1, status,
    canvas: status === "running" ? { url: `http://127.0.0.1:${port}/`, port } : null,
    pid: status === "stopped" ? null : 10, actions: [], log: null,
  };
}

test("plan keeps a running page at the same url, creates a new one, drops a stopped, closed or moved one", () => {
  const held = new Map([
    ["a", "http://127.0.0.1:1001/"], // still running there: kept
    ["b", "http://127.0.0.1:1002/"], // stopped: dropped
    ["c", "http://127.0.0.1:1003/"], // its page closed: dropped
    ["d", "http://127.0.0.1:1004/"], // moved to another port: dropped and made again
  ]);
  const p = plan(held, {
    pages: ["a", "b", "d", "e", "f"],
    rows: [row("a", "running", 1001), row("b", "stopped"), row("c", "running", 1003), row("d", "running", 1999), row("e", "running", 1005), row("f", "starting")],
  });
  assert.deepEqual(p.keep, ["a"]);
  assert.deepEqual(p.drop, ["b", "c", "d"]);
  assert.deepEqual(p.create, [
    ["d", "http://127.0.0.1:1999/"],
    ["e", "http://127.0.0.1:1005/"],
  ]);
  // nothing held, nothing open: nothing to do; a page with no row makes no frame
  assert.deepEqual(plan(new Map(), { pages: [], rows: [row("a", "running", 1)] }), { keep: [], create: [], drop: [] });
  assert.deepEqual(plan(new Map(), { pages: ["gone"], rows: [] }), { keep: [], create: [], drop: [] });
});

const lockState = (over: Record<string, unknown> = {}): unknown => ({
  available: true, locked: true, has_pin: true, auto_lock_min: 10, failures: 0, retry_at_ms: null, ...over,
});

test("the_lock_overlay_covers_the_page_until_unlock", () => {
  const NOW = 1_700_000_000_000;
  // nothing known, or an unlocked screen: no overlay
  assert.deepEqual(overlayFor(null, NOW), NO_OVERLAY);
  assert.equal(overlayFor(decodeLock(lockState({ locked: false })), NOW).shown, false);
  // a locked state draws it, with the PIN field open
  let overlay = overlayFor(decodeLock(lockState()), NOW);
  assert.deepEqual(overlay, { shown: true, message: "", canTry: true });
  // Escape and clicks outside the overlay's own field are swallowed, and change nothing
  for (const ev of [
    { type: "key" as const, key: "Escape", inside: false },
    { type: "key" as const, key: "w", inside: false },
    { type: "click" as const, inside: false },
  ]) {
    const r = overlayInput(overlay, ev);
    assert.equal(r.swallow, true, JSON.stringify(ev));
    assert.deepEqual(r.overlay, overlay, "the overlay is what only a lock state changes");
  }
  // typing in the PIN field is its own: not swallowed, and it does not dismiss either
  const typed = overlayInput(overlay, { type: "key", key: "Escape", inside: true });
  assert.deepEqual([typed.swallow, typed.overlay.shown], [false, true]);
  // a wrong PIN says so; five of them show the wait and close the field until it ends
  overlay = overlayFor(decodeLock(lockState({ failures: 2 })), NOW);
  assert.deepEqual(overlay, { shown: true, message: "Wrong PIN", canTry: true });
  overlay = overlayFor(decodeLock(lockState({ failures: 5, retry_at_ms: NOW + 29_500 })), NOW);
  assert.equal(overlay.shown && !overlay.canTry, true);
  assert.match(overlay.message, /try again in 30 s/);
  assert.equal(overlayFor(decodeLock(lockState({ failures: 5, retry_at_ms: NOW - 1 })), NOW).canTry, true, "the wait is over");
  // only a state that says unlocked removes it
  assert.deepEqual(overlayFor(decodeLock(lockState({ locked: false, failures: 0 })), NOW), NO_OVERLAY);
  // nothing else does: a reply that is not a lock state (a refusal) is not one
  assert.equal(decodeLock({ error: "unauthorized", reason: "unauthorized" }), null);
  assert.equal(decodeLock(null), null);
  assert.equal(decodeLock({ ...(lockState() as object), locked: "no" }), null);
  assert.equal(overlayInput(NO_OVERLAY, { type: "key", key: "Escape", inside: false }).swallow, false, "unlocked: the page keeps its keys");
});

test("the lock screen dials the running row named system, on its /user/ws, and sends activity once per ten seconds", () => {
  const rows = [row("a", "running", 1001), { ...row("sys", "running", 4242), name: "system" }, { ...row("old", "stopped"), name: "system" }];
  assert.equal(systemRow(rows)?.id, "sys");
  assert.equal(systemRow([row("a", "running", 1)]), undefined, "no row named system");
  assert.equal(systemRow([{ ...row("s", "stopped"), name: "system" }]), undefined, "a stopped one is not dialed");
  assert.equal(userSocketUrl("http://127.0.0.1:4242/"), "ws://127.0.0.1:4242/user/ws");
  assert.equal(userSocketUrl("https://h.example/x"), "wss://h.example/user/ws");
  assert.equal(userSocketUrl("not a url"), null);
  assert.equal(activityDue(null, 5), true);
  assert.equal(activityDue(1_000, 1_000 + ACTIVITY_EVERY_MS - 1), false);
  assert.equal(activityDue(1_000, 1_000 + ACTIVITY_EVERY_MS), true);
});
