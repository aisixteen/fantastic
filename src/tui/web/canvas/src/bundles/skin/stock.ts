import day from "./stock/day.tokens.json" with { type: "json" };
import dark from "./stock/dark.tokens.json" with { type: "json" };
import glass from "./stock/glass.tokens.json" with { type: "json" };

// THE STOCK SKINS — read-only, built in; the skin agent (`skin_agent.ts`) reads them.
//
//   day    web     the default: black and white glass over a light dotted page
//   dark   web     the earlier look, value for value
//   glass  native  translucent, for a window over a native vibrancy backdrop

export interface StockSkin {
  name: string;
  note: string;
  tokens: Record<string, unknown>;
}

export const STOCK_SKINS: ReadonlyArray<StockSkin> = [
  { name: "day", note: "black and white glass over a light page; colour only for state (the web default)", tokens: day as Record<string, unknown> },
  { name: "dark", note: "the canvas's own look: deep blue-black, a lilac accent", tokens: dark as Record<string, unknown> },
  { name: "glass", note: "translucent, over the window's native material (the app, macOS and Windows)", tokens: glass as Record<string, unknown> },
];

/** The stock skin each backdrop starts with. */
export const DEFAULT_SKINS = { web: "day", native: "glass" } as const;

export const stockSkin = (name: string): StockSkin | undefined => STOCK_SKINS.find((s) => s.name === name);
