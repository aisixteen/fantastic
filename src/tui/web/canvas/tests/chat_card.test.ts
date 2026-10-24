import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { installFakeDom, FakeEl } from "./fake_dom.ts";
import { CHAT_CARD_MAX_HEIGHT, ChatCards, cardBox, cardRows, cardState, cardsOf, hiddenKey } from "../src/bundles/ai_view/chat_card.ts";
import type { CardDeps } from "../src/bundles/ai_view/chat_card.ts";
import type { ContentSize } from "../src/bundles/canvas/fit.ts";
import { hiddenOf, prune, setHidden } from "../src/bundles/canvas/layout.ts";
import { Agent, Kernel } from "../src/lib.ts";

installFakeDom();

type Rec = Record<string, unknown>;
const PANEL: Rec = { id: "p1", view: { kind: "html", bytes: "<p>hi</p>", calls: ["get_html", "watch"] } };
const flush = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

/** A page for the controller: records, a hide set, and a log of what it was asked. */
function page(records: Record<string, Rec | undefined>, over: Partial<CardDeps> = {}) {
  const log: string[] = [];
  const hidden = new Set<string>();
  const sizers = new Map<string, (s: ContentSize) => void>();
  const unmounted: string[] = [];
  const deps: CardDeps = {
    chat: "chat1",
    recordOf: async (a) => records[a],
    mountPanel: (_el, agent, _rec, key) => {
      log.push(`mount ${agent}`);
      return { unmount: () => void unmounted.push(agent) };
    },
    hidden: { has: (k) => hidden.has(k), set: (k, on) => void (on ? hidden.add(k) : hidden.delete(k)) },
    unembed: async (a) => (log.push(`unembed ${a}`), { unembedded: a }),
    deleteAgent: (a) => void log.push(`delete ${a}`),
    expand: (a) => void log.push(`expand ${a}`),
    confirm: (m) => (log.push(`confirm ${m}`), answer.yes),
    toast: (m) => void log.push(`toast ${m}`),
    notify: (t) => void log.push(`notify ${t}`),
    onSize: (key, fn) => (sizers.set(key, fn), () => void sizers.delete(key)),
    ...over,
  };
  const answer = { yes: true };
  return { cards: new ChatCards(deps), log, hidden, sizers, unmounted, answer, records };
}
const click = (e: unknown, sel: string, init: Record<string, unknown> = {}): void => {
  (e as FakeEl).querySelector(sel)!.dispatch("click", init as never);
};
const card = (e: unknown): FakeEl => e as FakeEl;

test("chat_card_box_is_full_width_and_capped_at_1200", () => {
  assert.equal(CHAT_CARD_MAX_HEIGHT, 1200);
  assert.deepEqual(cardBox(300), { width: "100%", height: 300, scroll: false });
  assert.deepEqual(cardBox(5000), { width: "100%", height: 1200, scroll: true });
  assert.equal(cardBox(3).height, 60, "never below the fit minimum");
});

test("the TS cap says what ai-core's says", () => {
  const rust = readFileSync(new URL("../../../../lib/rust/crates/bundles/fantastic-ai-core/src/constants.rs", import.meta.url), "utf8");
  assert.match(rust, /pub const CHAT_CARD_MAX_HEIGHT: i64 = 1200;/);
});

test("an_embed_row_redraws_the_same_card_after_a_reload", async () => {
  const rows = [
    { id: 1, kind: undefined, role: "user", head: "make me a dashboard" },
    { id: 2, kind: "embed", head: "p1" },
    { id: 3, role: "assistant", head: "here it is" },
    { id: 4, kind: "embed", head: "p2" },
    { id: 5, kind: "unembed", head: "p2" },
  ];
  assert.deepEqual(cardsOf(rows), ["p1"], "p2 was taken out");
  assert.deepEqual([...cardRows(rows)], [["p1", 2]], "the card sits at its embed row");
  // two mounts of the same history draw the same card, with the same agent
  for (let mount = 0; mount < 2; mount++) {
    const p = page({ p1: PANEL });
    const els = [...cardRows(rows).keys()].map((a) => p.cards.draw(a));
    await flush();
    assert.equal(els.length, 1);
    assert.equal(card(els[0]).dataset["agent"], "p1");
    assert.equal(card(els[0]).dataset["state"], "live");
    assert.deepEqual(p.log, ["mount p1"]);
  }
  // an embed after an unembed shows it again, at the new row
  assert.deepEqual([...cardRows([...rows, { id: 6, kind: "embed", head: "p2" }])], [["p1", 2], ["p2", 6]]);
});

test("a_removed_frame_turns_the_card_deleted_and_the_press_rechecks", async () => {
  const p = page({ p1: PANEL });
  const e = p.cards.draw("p1");
  await flush();
  assert.equal(card(e).dataset["state"], "live");
  // a frame about another agent changes nothing
  p.cards.onState({ kind: "removed", agent_id: "other" });
  assert.equal(card(e).dataset["state"], "live");
  p.cards.onState({ kind: "removed", agent_id: "p1" });
  assert.equal(card(e).dataset["state"], "deleted");
  assert.deepEqual(p.unmounted, ["p1"], "its panel is torn down");
  assert.equal(card(e).querySelector(".c-card-deleted")!.textContent, "[deleted from canvas]");

  // gone → the press asks; yes → unembed {id}, the card leaves
  p.records["p1"] = undefined;
  click(e, ".c-card-deleted");
  await flush();
  assert.ok(p.log.some((l) => l.startsWith("confirm ")));
  assert.ok(p.log.includes("unembed p1"));
  assert.equal(p.cards.has("p1"), false);

  // back under the same id → the press re-attaches, no question
  const q = page({ p1: undefined });
  const e2 = q.cards.draw("p1");
  await flush();
  assert.equal(card(e2).dataset["state"], "deleted", "a redraw with the agent missing");
  assert.equal(card(e2).querySelector(".c-card-deleted")!.textContent, "[deleted from canvas]");
  q.records["p1"] = PANEL;
  click(e2, ".c-card-deleted");
  await flush();
  assert.equal(card(e2).dataset["state"], "live");
  assert.ok(!q.log.some((l) => l.startsWith("confirm")), "no question when it is back");

  // gone, and the user says no → the card stays
  const r = page({ p1: undefined });
  r.answer.yes = false;
  const e3 = r.cards.draw("p1");
  await flush();
  click(e3, ".c-card-deleted");
  await flush();
  assert.equal(r.cards.has("p1"), true);
  assert.ok(!r.log.includes("unembed p1"));
});

test("hide_keeps_the_agent_and_shift_x_deletes_with_the_lock", async () => {
  const p = page({ p1: PANEL });
  const e = p.cards.draw("p1");
  await flush();
  click(e, ".c-card-x");
  assert.equal(card(e).dataset["state"], "hidden");
  assert.ok(p.hidden.has(hiddenKey("chat1", "p1")));
  assert.ok(!p.log.some((l) => l.startsWith("delete")), "hide never deletes");
  // stays hidden after a remount of the same chat
  const p2 = page({ p1: PANEL });
  p2.hidden.add(hiddenKey("chat1", "p1"));
  const e2 = p2.cards.draw("p1");
  await flush();
  assert.equal(card(e2).dataset["state"], "hidden");
  assert.deepEqual(p2.log, [], "a hidden card mounts no panel");
  click(e2, ".c-card-show");
  await flush();
  assert.equal(card(e2).dataset["state"], "live");

  // Shift+x with the delete lock: the toast, no delete
  const locked = page({ p1: { ...PANEL, delete_lock: true } });
  const e3 = locked.cards.draw("p1");
  await flush();
  click(e3, ".c-card-x", { shiftKey: true });
  assert.ok(locked.log.some((l) => l.startsWith("toast ✕ p1 is locked")));
  assert.ok(!locked.log.some((l) => l.startsWith("delete")));
  // without the lock: the kernel's delete_agent path
  const free = page({ p1: PANEL });
  const e4 = free.cards.draw("p1");
  await flush();
  click(e4, ".c-card-x", { shiftKey: true });
  assert.ok(free.log.includes("delete p1"));
  assert.equal(card(e4).dataset["state"], "live", "the card turns deleted on the removed frame, not on the press");
  // hide on the canvas is the cell's key in the view's set; the card's key is its own
  const k = new Kernel();
  k.register(new Agent({ id: "view", handlerModule: "x", meta: {} as never }));
  k.register(new Agent({ id: "chat1", parentId: "view", handlerModule: "x", meta: {} as never }));
  setHidden(k, "view", "cellOfSameAgent", true);
  setHidden(k, "view", hiddenKey("chat1", "p1"), true);
  setHidden(k, "view", "cellOfSameAgent", false);
  assert.ok(hiddenOf(k, "view").has(hiddenKey("chat1", "p1")), "hiding/showing a canvas cell leaves the chat card's hide");
  // the layout's prune keeps a live chat's card keys and drops a dead chat's
  setHidden(k, "view", hiddenKey("gone_chat", "p1"), true);
  prune(k, "view", new Set(["chat1"]));
  assert.deepEqual([...hiddenOf(k, "view")], [hiddenKey("chat1", "p1")]);
});

test("unembed_sends_and_never_touches_the_agent", async () => {
  const p = page({ p1: PANEL });
  const e = p.cards.draw("p1");
  await flush();
  click(e, ".c-card-remove");
  await flush();
  assert.deepEqual(p.log, ["mount p1", "unembed p1"], "only unembed went out");
  assert.equal(p.cards.has("p1"), false);
  assert.deepEqual(p.unmounted, ["p1"]);
  // a refusal keeps the card and says so
  const bad = page({ p1: PANEL }, { unembed: async () => ({ error: "no store" }) });
  const e2 = bad.cards.draw("p1");
  await flush();
  click(e2, ".c-card-remove");
  await flush();
  assert.equal(bad.cards.has("p1"), true);
  assert.ok(bad.log.includes("toast ✕ no store"));
});

test("the card follows its panel's height, capped, and a fresh overflow is told to the AI", async () => {
  const p = page({ p1: PANEL });
  const e = p.cards.draw("p1", true);
  await flush();
  const body = card(e).querySelector(".c-card-body")!;
  p.sizers.get("card:chat1:p1")!({ mw: 300, h: 340 });
  assert.equal(body.style["height"], "340px");
  assert.equal(body.style["width"], "100%");
  assert.ok(!p.log.some((l) => l.startsWith("notify")), "a card that fits is not told anything");
  // only a fresh card's FIRST report is judged
  p.sizers.get("card:chat1:p1")!({ mw: 300, h: 5000 });
  assert.equal(body.style["height"], "1200px");
  assert.equal(body.style["overflow"], "auto");
  assert.ok(!p.log.some((l) => l.startsWith("notify")));

  const big = page({ p1: PANEL });
  const e2 = big.cards.draw("p1", true);
  await flush();
  big.sizers.get("card:chat1:p1")!({ mw: 300, h: 3000 });
  assert.equal(big.log.filter((l) => l.startsWith("notify")).length, 1);
  assert.match(big.log.find((l) => l.startsWith("notify"))!, /\[chat card p1\].*3000 px.*1200 px.*Redo the panel/);
  big.sizers.get("card:chat1:p1")!({ mw: 300, h: 3100 });
  assert.equal(big.log.filter((l) => l.startsWith("notify")).length, 1, "told once");
  assert.equal(card(e2).querySelector(".c-card-body")!.style["height"], "1200px");

  const wide = page({ p1: PANEL });
  wide.cards.draw("p1", true);
  await flush();
  wide.sizers.get("card:chat1:p1")!({ mw: 2000, h: 200, ox: true });
  assert.match(wide.log.find((l) => l.startsWith("notify"))!, /wider than the chat column/);

  // a card redrawn from history (not fresh) says nothing
  const old = page({ p1: PANEL });
  old.cards.draw("p1", false);
  await flush();
  old.sizers.get("card:chat1:p1")!({ mw: 300, h: 3000 });
  assert.ok(!old.log.some((l) => l.startsWith("notify")));
});

test("cardState: a missing record is deleted; hide is hide", () => {
  assert.equal(cardState(undefined, false), "deleted");
  assert.equal(cardState({ id: "x" }, false), "deleted", "a record with no view is no panel");
  assert.equal(cardState(PANEL, false), "live");
  assert.equal(cardState(PANEL, true), "hidden");
});
