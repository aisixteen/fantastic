import { STOCK, sourceOf } from "../backgrounds/stock.ts";

// THE BUNDLED BACKGROUND (owner 2026-10-09): Isoline in its Ember set — the
// first stock background (`backgrounds/stock.ts`). `default_bg` restores it, and
// a view's background starts with it.
const ISOLINE = STOCK.find((s) => s.name === "Isoline")!;
export const DEFAULT_BG_SOURCE = sourceOf(ISOLINE.body, ISOLINE.sets.find(([n]) => n === "Ember")![1]);
