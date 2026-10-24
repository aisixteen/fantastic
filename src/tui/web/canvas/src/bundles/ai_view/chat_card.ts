import { FIT_MIN, forgetSize, onSize } from "../canvas/fit.ts";
import type { ContentSize } from "../canvas/fit.ts";
import type { ViewHandle } from "../view.ts";

// CHAT CARDS (#339): a host agent's panel shown INLINE in a chat. The card is a row of
// the chat's conversation (`embed {id}` on the AI, replayed by `history` as a row of
// kind `embed`); this page draws it where the row sits, re-attached live to the agent
// the way a canvas cell re-attaches after a reload. This file is the pure model
// (`cardsOf`, `cardBox`, `cardState`) and the controller that owns the cards of ONE chat
// cell (`ChatCards`); `ai_view.ts` wires it to the dialog, the kernel and the canvas.

/** The tallest a card gets, in CSS px — content taller than this scrolls inside the card.
 *  The TS twin of ai-core's `CHAT_CARD_MAX_HEIGHT` (a test reads both). */
export const CHAT_CARD_MAX_HEIGHT = 1200;

/** A card's height until its panel has reported one. */
export const CARD_START_HEIGHT = 200;

type Rec = Record<string, unknown>;

/** The agents whose cards are on show, oldest first, from the chat's history digests:
 *  an `embed` row adds its agent (its place is that row's), an `unembed` row removes it.
 *  The agent id is the digest's `head`. */
export function cardsOf(rows: Rec[]): string[] {
  const out: string[] = [];
  for (const m of rows) {
    const kind = m["kind"];
    if (kind !== "embed" && kind !== "unembed") continue;
    const id = m["head"];
    if (typeof id !== "string" || id === "") continue;
    const at = out.indexOf(id);
    if (at >= 0) out.splice(at, 1);
    if (kind === "embed") out.push(id);
  }
  return out;
}

/** For each agent on show, the id of the history row that puts its card in its place. */
export function cardRows(rows: Rec[]): Map<string, number> {
  const out = new Map<string, number>();
  for (const m of rows) {
    const kind = m["kind"];
    const id = m["head"];
    if ((kind !== "embed" && kind !== "unembed") || typeof id !== "string") continue;
    if (kind === "embed" && typeof m["id"] === "number") out.set(id, m["id"] as number);
    else out.delete(id);
  }
  return out;
}

/** Is `m` a history row that is a card (not a turn)? */
export const isCardRow = (m: Rec): boolean => m["kind"] === "embed" || m["kind"] === "unembed";

/** The card's box around a panel that measured `measured` px tall: the column's whole
 *  width, a defined height clamped to [FIT_MIN, CHAT_CARD_MAX_HEIGHT], and whether the
 *  content is taller than the cap (then it scrolls inside the card). */
export function cardBox(measured: number): { width: "100%"; height: number; scroll: boolean } {
  const h = Number.isFinite(measured) ? Math.ceil(measured) : CARD_START_HEIGHT;
  return { width: "100%", height: Math.min(Math.max(h, FIT_MIN.height), CHAT_CARD_MAX_HEIGHT), scroll: h > CHAT_CARD_MAX_HEIGHT };
}

export type CardState = "live" | "deleted" | "hidden";

/** The key a card's hide state is filed under, in the chat cell's view (`layout.ts`). */
export const hiddenKey = (chat: string, agent: string): string => `embed:${chat}:${agent}`;

/** What a card shows: a missing host record is a DELETED agent; a hidden card is hidden
 *  (✕ — the agent keeps running); else live. `hide` and `remove` on the canvas never reach
 *  here: only the record's absence does. */
export function cardState(rec: Rec | undefined, hidden: boolean): CardState {
  if (!hasView(rec)) return "deleted";
  return hidden ? "hidden" : "live";
}

/** Does the host record carry a `view` — a panel a card can draw? */
export function hasView(rec: Rec | undefined): boolean {
  return rec !== undefined && rec["view"] !== null && typeof rec["view"] === "object";
}

/** Everything the controller needs from its page — injected, so it runs without a canvas. */
export interface CardDeps {
  /** the chat cell's id (cards are filed under it) */
  chat: string;
  /** the host record of `agent`, read again from the host when the page has none */
  recordOf(agent: string): Promise<Rec | undefined>;
  /** draw `agent`'s panel into `el`; calls go out as the chat cell, granted from the record */
  mountPanel(el: HTMLElement, agent: string, rec: Rec, sizeKey: string): ViewHandle;
  /** hide state, filed in the chat cell's view */
  hidden: { has(key: string): boolean; set(key: string, on: boolean): void };
  /** `unembed {id}` on the chat's AI */
  unembed(agent: string): Promise<unknown>;
  /** the kernel's `delete_agent` (its own lock gate refuses what is locked) */
  deleteAgent(agent: string): void;
  /** the agent full screen */
  expand(agent: string): void;
  confirm(message: string): boolean;
  toast(message: string): void;
  /** a short note to the chat's AI (through its `send`) */
  notify(text: string): void;
  onSize?: (key: string, fn: (s: ContentSize) => void) => () => void;
}

interface Card {
  agent: string;
  el: HTMLElement;
  rec: Rec | undefined;
  panel: ViewHandle | null;
  offSize: (() => void) | null;
  sizeKey: string;
  /** drawn for an embed that just arrived: its first size report is judged */
  fresh: boolean;
}

const el = (tag: string, cls: string, text?: string): HTMLElement => {
  const e = document.createElement(tag);
  e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
};

/** The cards of one chat cell. */
export class ChatCards {
  private readonly deps: CardDeps;
  private readonly cards = new Map<string, Card>();

  constructor(deps: CardDeps) {
    this.deps = deps;
  }

  has(agent: string): boolean {
    return this.cards.has(agent);
  }

  agents(): string[] {
    return [...this.cards.keys()];
  }

  /** The card element for `agent` (made once; the caller puts it in the dialog). It fills
   *  in as soon as the host record is read. `fresh`: an embed that just arrived. */
  draw(agent: string, fresh = false): HTMLElement {
    const have = this.cards.get(agent);
    if (have !== undefined) return have.el;
    const root = el("div", "c-card");
    root.dataset["agent"] = agent;
    root.dataset["state"] = "loading";
    const c: Card = { agent, el: root, rec: undefined, panel: null, offSize: null, sizeKey: `card:${this.deps.chat}:${agent}`, fresh };
    this.cards.set(agent, c);
    void this.attach(c);
    return root;
  }

  /** Take `agent`'s card out of the dialog (its row was unembedded). */
  remove(agent: string): void {
    const c = this.cards.get(agent);
    if (c === undefined) return;
    this.teardown(c);
    c.el.remove();
    this.cards.delete(agent);
  }

  /** The host's state stream: an agent REMOVED turns its card deleted at once. */
  onState(frame: Rec): void {
    if (frame["kind"] !== "removed" || typeof frame["agent_id"] !== "string") return;
    const c = this.cards.get(frame["agent_id"]);
    if (c !== undefined) this.paint(c, "deleted", undefined);
  }

  /** The press on a deleted card: is the agent really gone, or back under the same id? */
  async recheck(agent: string): Promise<void> {
    const c = this.cards.get(agent);
    if (c === undefined) return;
    const rec = await this.deps.recordOf(agent);
    if (this.cards.get(agent) !== c) return;
    if (hasView(rec)) {
      this.paint(c, cardState(rec, this.deps.hidden.has(hiddenKey(this.deps.chat, agent))), rec);
      return;
    }
    if (this.deps.confirm(`${agent} is gone. Remove this card?`)) await this.unembed(agent);
  }

  dispose(): void {
    for (const c of this.cards.values()) this.teardown(c);
    this.cards.clear();
  }

  // ── the card's actions ───────────────────────────────────────────────

  /** ✕ hides the card (the agent keeps running); Shift+✕ deletes the agent. */
  closeCard(agent: string, shift: boolean): void {
    if (shift) return this.deleteCard(agent);
    const c = this.cards.get(agent);
    if (c === undefined) return;
    this.deps.hidden.set(hiddenKey(this.deps.chat, agent), true);
    this.paint(c, "hidden", c.rec);
  }

  /** Delete the card's AGENT — the canvas's rule (#133): a delete_lock refuses, said. */
  deleteCard(agent: string): void {
    const c = this.cards.get(agent);
    if (c?.rec !== undefined && c.rec["delete_lock"] === true) {
      this.deps.toast(`✕ ${agent} is locked — unlock it to delete`);
      return;
    }
    this.deps.deleteAgent(agent);
  }

  /** Take the card out of the chat: `unembed {id}` on the AI; the agent is not touched. */
  async unembed(agent: string): Promise<void> {
    const r = (await this.deps.unembed(agent).catch((e: Error) => ({ error: e.message }))) as Rec | null;
    if (r !== null && typeof r["error"] === "string") {
      this.deps.toast(`✕ ${r["error"]}`);
      return;
    }
    this.remove(agent);
  }

  // ── drawing ─────────────────────────────────────────────────────────────

  private async attach(c: Card): Promise<void> {
    const rec = await this.deps.recordOf(c.agent);
    if (this.cards.get(c.agent) !== c) return;
    this.paint(c, cardState(rec, this.deps.hidden.has(hiddenKey(this.deps.chat, c.agent))), rec);
  }

  private teardown(c: Card): void {
    c.panel?.unmount();
    c.panel = null;
    c.offSize?.();
    c.offSize = null;
    forgetSize(c.sizeKey);
  }

  private paint(c: Card, state: CardState, rec: Rec | undefined): void {
    this.teardown(c);
    c.rec = rec;
    c.el.dataset["state"] = state;
    c.el.replaceChildren();
    if (state === "deleted") {
      const b = el("button", "c-card-deleted", "[deleted from canvas]");
      b.title = `${c.agent} is gone — press to check again`;
      b.addEventListener("click", () => void this.recheck(c.agent));
      c.el.appendChild(b);
      return;
    }
    if (state === "hidden") {
      const bar = el("div", "c-card-hiddenbar");
      bar.appendChild(el("span", "c-card-title", `${c.agent} (hidden)`));
      const show = el("button", "c-card-show", "show");
      show.addEventListener("click", () => {
        this.deps.hidden.set(hiddenKey(this.deps.chat, c.agent), false);
        void this.attach(c);
      });
      bar.appendChild(show);
      c.el.appendChild(bar);
      return;
    }
    // live: a head, and the panel at the column's full width
    const head = el("div", "c-card-head");
    head.appendChild(el("span", "c-card-title", c.agent));
    const expand = el("button", "c-card-expand", "⤢");
    expand.title = "open this agent full screen";
    expand.addEventListener("click", () => this.deps.expand(c.agent));
    const menu = el("div", "c-card-menu");
    menu.dataset["open"] = "";
    const remove = el("button", "c-card-remove", "remove card");
    remove.title = "take the card out of this chat — the agent keeps running";
    remove.addEventListener("click", () => void this.unembed(c.agent));
    const del = el("button", "c-card-delete", "delete agent");
    del.title = "delete the agent (a locked one is refused)";
    del.addEventListener("click", () => this.deleteCard(c.agent));
    menu.appendChild(remove);
    menu.appendChild(del);
    const more = el("button", "c-card-more", "⋯");
    more.title = "card menu";
    more.addEventListener("click", () => {
      menu.dataset["open"] = menu.dataset["open"] === "1" ? "" : "1";
    });
    const x = el("button", "c-card-x", "×");
    x.title = "hide — the agent keeps running (Shift+×: delete it)";
    x.addEventListener("click", (e) => this.closeCard(c.agent, (e as MouseEvent).shiftKey === true));
    for (const n of [expand, more, x, menu]) head.appendChild(n);
    const body = el("div", "c-card-body");
    body.style.width = "100%";
    body.style.height = `${CARD_START_HEIGHT}px`;
    c.el.appendChild(head);
    c.el.appendChild(body);
    const subscribe = this.deps.onSize ?? onSize;
    c.offSize = subscribe(c.sizeKey, (s) => this.sized(c, body, s));
    c.panel = this.deps.mountPanel(body, c.agent, rec!, c.sizeKey);
  }

  /** The panel reported its size: the card follows it, capped; the first report of a fresh card
   *  that is over the cap, or wider than the column, is told to the AI. */
  private sized(c: Card, body: HTMLElement, s: ContentSize): void {
    const box = cardBox(s.h);
    body.style.height = `${box.height}px`;
    body.style.overflow = box.scroll ? "auto" : "hidden";
    body.dataset["scroll"] = box.scroll ? "1" : "";
    if (!c.fresh) return;
    c.fresh = false;
    if (s.h <= CHAT_CARD_MAX_HEIGHT && s.ox !== true) return;
    const why = s.ox === true ? "its content is wider than the chat column" : `its content is ${s.h} px tall, over the ${CHAT_CARD_MAX_HEIGHT} px cap`;
    this.deps.notify(
      `[chat card ${c.agent}] ${why}. Redo the panel for the chat window: fill 100% of the width (set no width) and define its height, at most ${CHAT_CARD_MAX_HEIGHT} px.`,
    );
  }
}
