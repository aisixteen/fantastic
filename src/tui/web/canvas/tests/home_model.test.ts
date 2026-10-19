import { test } from "node:test";
import assert from "node:assert/strict";
import { STATUSES, cardVerbs, colourVar, decodeState, label, lastSeen, shortcut, stateCard } from "../src/bundles/home_view/home_model.ts";
import type { HomeRow, HomeState, KeyLike, Status } from "../src/bundles/home_view/home_model.ts";
import { HOME_CSS } from "../src/bundles/home_view/home_css.ts";

const NOW = 1_800_000_000_000;

function row(status: Status, over: Partial<HomeRow> = {}): HomeRow {
  return {
    id: "g-1", guid: "g-1", name: "alpha", path: "/work/alpha", last_seen: NOW / 1000 - 7200, status,
    canvas: status === "running" ? { url: "http://127.0.0.1:4100/", port: 4100 } : null,
    pid: status === "stopped" || status === "missing" ? null : 42, actions: [], log: null, ...over,
  };
}

/** A state as the home sends it (`home/wire.rs`). */
function wire(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    type: "state", protocol: "fantastic.home", version: 1, seq: 7, mode: "lock", listed: true,
    rows: [row("running", { actions: ["stop", "open", "remove"] }), { ...row("stopped"), id: "adding:/x", guid: null, last_seen: null, log: "/h/logs/x.log" }],
    pages: ["g-1"], selection: "g-1", problem: null, ...over,
  };
}

test("decodeState accepts the home's state and names what is wrong otherwise", () => {
  const ok = decodeState(wire());
  assert.notEqual(typeof ok, "string", String(ok));
  assert.equal((ok as HomeState).rows[0]!.canvas!.port, 4100);
  assert.equal(typeof decodeState(wire({ problem: "it broke", selection: null, mode: "canvas" })), "object");

  const why = (over: Record<string, unknown>): string => {
    const r = decodeState(wire(over));
    assert.equal(typeof r, "string", JSON.stringify(over));
    return r as string;
  };
  assert.match(why({ protocol: "fantastic.canvas" }), /protocol/);
  assert.match(why({ version: 2 }), /version/);
  assert.equal(why({ rows: undefined }), "state: `rows` is not a list");
  assert.match(why({ rows: [row("sleeping" as Status)] }), /rows\[0\]\.status: "sleeping" is not a status/);
  assert.match(why({ type: "focus" }), /type/);
  assert.match(why({ seq: "7" }), /seq/);
  assert.match(why({ pages: [1] }), /pages/);
  assert.match(why({ rows: [row("running", { actions: ["explode" as never] })] }), /actions/);
  assert.match(why({ rows: [row("running", { canvas: { url: 1 } as never })] }), /canvas/);
  assert.match(decodeState(null) as string, /not an object/);
});

test("every status has a label, a colour variable and a state card", () => {
  const labels = new Set<string>();
  for (const s of STATUSES) {
    assert.ok(label(s).length > 0, s);
    labels.add(label(s));
    assert.equal(colourVar(s), `--home-status-${s}`);
    assert.ok(HOME_CSS.includes(`${colourVar(s)}:`), `home_css sets ${colourVar(s)}`);
    const c = stateCard(row(s), NOW);
    if (s === "running") assert.equal(c, null, "a running page shows its canvas");
    else assert.ok(c !== null && c.title.length > 0 && c.detail.length > 0, s);
  }
  assert.equal(labels.size, STATUSES.length, "no two statuses share a label");
  assert.equal(label("missing"), "Folder not found");
  assert.equal(label("no_canvas"), "Running without a canvas");
  assert.equal(stateCard(row("starting"), NOW)!.title, "Starting alpha…");
  assert.equal(stateCard(row("stopped"), NOW)!.detail, "Stopped · last seen 2 hours ago");
  assert.equal(stateCard(row("stopped", { last_seen: 0 }), NOW)!.detail, "Stopped");
  assert.equal(stateCard(row("missing"), NOW)!.detail, "/work/alpha is gone, or no longer holds a fantastic.");
  assert.equal(lastSeen(NOW / 1000 - 30, NOW), "last seen 30 seconds ago");
  assert.equal(lastSeen(NOW / 1000 - 3 * 86400, NOW), "last seen 3 days ago");
});

test("a state card's buttons are its verbs that the row's actions offer", () => {
  assert.deepEqual(cardVerbs(row("stopped", { actions: ["start", "open", "remove"] })), ["start"]);
  assert.deepEqual(cardVerbs(row("stopped", { actions: ["open"] })), [], "canvas mode: no start");
  assert.deepEqual(cardVerbs(row("no_canvas", { actions: ["stop", "open", "remove"] })), ["stop"]);
  assert.deepEqual(cardVerbs(row("unverified", { actions: ["open"] })), []);
  assert.deepEqual(cardVerbs(row("missing", { actions: ["open", "remove"] })), ["remove"]);
  assert.deepEqual(cardVerbs(row("starting", { actions: ["stop", "open"] })), []);
  assert.deepEqual(cardVerbs(row("running", { actions: ["stop", "open", "remove"] })), []);
});

test("shortcuts: mod+0 home, mod+1..9 a page in range, mod+w the shown page; others and out-of-range are nothing; meta on a Mac, ctrl elsewhere", () => {
  const state = { pages: ["a", "b"], selection: "b" as string | null };
  const k = (key: string, mods: Partial<KeyLike> = {}): KeyLike => ({ key, metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, ...mods });
  for (const [isMac, mod, other] of [[true, "metaKey", "ctrlKey"], [false, "ctrlKey", "metaKey"]] as const) {
    const m = { [mod]: true };
    assert.deepEqual(shortcut(k("0", m), isMac, state), { verb: "select", args: { id: null } });
    assert.deepEqual(shortcut(k("1", m), isMac, state), { verb: "select_page", args: { index: 0 } });
    assert.deepEqual(shortcut(k("2", m), isMac, state), { verb: "select_page", args: { index: 1 } });
    assert.equal(shortcut(k("3", m), isMac, state), null, "out of range");
    assert.equal(shortcut(k("9", m), isMac, state), null, "out of range");
    assert.deepEqual(shortcut(k("w", m), isMac, state), { verb: "close_page", args: { id: "b" } });
    assert.deepEqual(shortcut(k("W", m), isMac, state), { verb: "close_page", args: { id: "b" } });
    assert.equal(shortcut(k("w", m), isMac, { ...state, selection: null }), null, "the Home has no page to close");
    assert.equal(shortcut(k("1"), isMac, state), null, "no modifier");
    assert.equal(shortcut(k("1", { [other]: true }), isMac, state), null, "the other platform's modifier");
    assert.equal(shortcut(k("1", { ...m, [other]: true }), isMac, state), null);
    assert.equal(shortcut(k("1", { ...m, altKey: true }), isMac, state), null);
    assert.equal(shortcut(k("1", { ...m, shiftKey: true }), isMac, state), null);
    assert.equal(shortcut(k("x", m), isMac, state), null);
    assert.equal(shortcut(k("Escape", m), isMac, state), null);
  }
});
