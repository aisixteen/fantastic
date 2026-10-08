import { str } from "../../lib.ts";
import type { Handler, Json } from "../../lib.ts";

// THE SELECTOR — the double-click's popup, as its own agent (owner 2026-10-08):
// EPHEMERAL. A double click on empty canvas (or ⌘K / Ctrl-K) ADDS one to the
// viewed view, at the spot (`x`, `y` in the view's world; `sx`, `sy` on the
// screen); its boot opens the popup ON TOP of everything; the human picks (or
// escapes, or clicks away) and it DELETES ITSELF. While it lives, anyone may ask
// it what can be made there (`tiles {typed?}`). It is never a cell (the canvas
// draws no frame for it) and it never offers itself.

export const SELECTOR = "selector.ts";

/** A selector's id: `selector_<hex8>` (one per double click). */
export const SELECTOR_PREFIX = "selector_";

/** The title it shows unless its record names another. */
export const SELECTOR_TITLE = "Fantastic Canvas";

export const SELECTOR_README =
  "THE SELECTOR — the canvas's double-click popup, as an agent: added at the spot by a double click, it deletes itself once the human picks. " +
  "`open` (its boot) shows it on top — what the human then picks is made at its spot. " +
  "`tiles {typed?}` answers what it offers for that text: `{tiles: [{label, kind, section, hint}]}` — every kind a cell can be made of here. " +
  "Its `title` field is the popup's title.";

/** What the canvas lends the selector: the popup itself, and its tiles. */
export interface SelectorHost {
  /** show selector `id`'s popup at its screen point; `done` runs once it is gone */
  open(id: string, sx: number | null, sy: number | null, title: string, done: () => void): Promise<void>;
  tiles(typed: string): Promise<Array<{ label: string; kind: string; section: string; hint: string }>>;
}
let lent: SelectorHost | null = null;
export function setSelectorHost(h: SelectorHost): void {
  lent = h;
}

export const selectorAgent: Handler = async (id, payload, kernel): Promise<Json> => {
  const title = (): string => {
    const t = kernel.get(id)?.meta["title"];
    return typeof t === "string" && t !== "" ? t : SELECTOR_TITLE;
  };
  const num = (k: string): number | null => (typeof payload[k] === "number" ? (payload[k] as number) : null);
  switch (str(payload, "type")) {
    case "boot":
    case "open": {
      if (lent === null) return { error: "selector: no canvas is mounted on this page", reason: "not_found" };
      const m = kernel.get(id)?.meta ?? {};
      const at = (k: string): number | null => (typeof m[k] === "number" ? (m[k] as number) : num(k === "sx" ? "x" : "y"));
      // picked, escaped or clicked away: it deletes itself
      await lent.open(id, at("sx"), at("sy"), title(), () => {
        if (kernel.get(id) !== undefined) kernel.remove(id);
      });
      return { opened: true, title: title() };
    }
    case "tiles":
      if (lent === null) return { error: "selector: no canvas is mounted on this page", reason: "not_found" };
      return { tiles: await lent.tiles(str(payload, "typed")) } as unknown as Json;
    case "shutdown":
      return null;
    default:
      return {
        error: `selector: unknown verb '${str(payload, "type")}'`,
        reason: "unhandled",
        verb: str(payload, "type"),
        hint: "the selector answers open {x?, y?} and tiles {typed?}; its title is its record's `title`",
      };
  }
};

export function registerSelector(kernel: {
  registerBundle(hm: string, h: Handler, verbs?: Record<string, string>): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(SELECTOR, selectorAgent, {
    open: "No args (its boot runs it). Shows its popup on top, at its spot; it deletes itself once the human picks, escapes or clicks away. {opened, title}.",
    tiles: "args: typed:str? (what the human typed). {tiles: [{label, kind, section, hint}]}: what the popup offers for it.",
  });
  kernel.setBundleReadme(SELECTOR, SELECTOR_README);
}
