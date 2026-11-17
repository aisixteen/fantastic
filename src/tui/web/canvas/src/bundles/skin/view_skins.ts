import day from "./stock/day.tokens.json" with { type: "json" };
import dark from "./stock/view_dark.tokens.json" with { type: "json" };
import transparent from "./stock/view_transparent.tokens.json" with { type: "json" };
import { compileSkin } from "./tokens.ts";

// THE SKINS OF AN HTML VIEW (#698) — an agent's `view {kind: html}` runs in a sandboxed
// frame and declares no colour of its own: every value it draws with is a skin token
// (`var(--fc-…)`), and the HOST that mounts it passes the token block of the skin it
// shows (`mountView({skin})` in `web/page/view_relay.js`), and the new block on a change.
//
//   light        the team design mock's light colours: the day skin, value for value
//   dark         the mock's dark colours; what the mock has none for, the canvas's dark skin's
//   transparent  for a view on the canvas: the page's backdrop shows through (page.bg alpha 0);
//                the canvas's day colours, the tree's hover and selection as before the #608 restyle
//
// The blocks are also written to `web/page/view_skins/<name>.css` (`scripts/view_skins.ts`),
// for a host that does not compile tokens (a product page served from Rust).

export const VIEW_SKINS = ["light", "dark", "transparent"] as const;
export type ViewSkin = (typeof VIEW_SKINS)[number];

const TOKENS: Record<ViewSkin, Record<string, unknown>> = {
  light: day as Record<string, unknown>,
  dark: dark as Record<string, unknown>,
  transparent: transparent as Record<string, unknown>,
};

/** A view skin's token document (as authored). */
export const viewSkinTokens = (name: ViewSkin): Record<string, unknown> => TOKENS[name];

/** A view skin's token block: `:root { --fc-…: …; }`, every schema token. */
export function viewSkinBlock(name: ViewSkin): string {
  const c = compileSkin(TOKENS[name]);
  if (!c.ok) throw new Error(`[skin] the view skin ${name} does not compile: ${c.error}`);
  return c.skin.text;
}
