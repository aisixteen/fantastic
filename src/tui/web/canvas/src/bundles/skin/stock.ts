import dark from "./stock/dark.tokens.json" with { type: "json" };
import glass from "./stock/glass.tokens.json" with { type: "json" };

// THE STOCK SKINS — read-only, built in. A user skin is merged over one of them
// (or over another user skin): `skin_agent.ts`.
//
//   dark   web     today's look, value for value
//   glass  native  translucent, for a window over a native vibrancy backdrop

export interface StockSkin {
  name: string;
  note: string;
  tokens: Record<string, unknown>;
}

export const STOCK_SKINS: ReadonlyArray<StockSkin> = [
  { name: "dark", note: "the canvas's own look: deep blue-black, a lilac accent", tokens: dark as Record<string, unknown> },
  { name: "glass", note: "translucent, over the window's native material (the app, macOS and Windows)", tokens: glass as Record<string, unknown> },
];

/** The stock skin each backdrop starts with. */
export const DEFAULT_SKINS = { web: "dark", native: "glass" } as const;

export const stockSkin = (name: string): StockSkin | undefined => STOCK_SKINS.find((s) => s.name === name);
