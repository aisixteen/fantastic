import { SCHEMA_BY_PATH, cssName } from "./schema.ts";

// HOW A CSS MODULE READS THE SKIN: in its template string, `${tok(path)}` for a
// token's value and `${tint(path, pct)}` for an opaque colour token at an alpha.
// Both throw AT MODULE LOAD on a path the schema does not name, so a typo never
// ships a style that silently falls back.
//
//   tok("color.surface.bezel")    → "var(--fc-color-surface-bezel)"
//   tint("color.accent.base", 16) → "color-mix(in srgb, var(--fc-color-accent-base) 16%, transparent)"
//
// `tint(…, a×100)` reproduces `rgba(r,g,b,a)` exactly: color-mix interpolates
// with premultiplied alpha, so mixing with `transparent` keeps the hue and sets
// the alpha to a (Chrome 111, Safari 16.2, Firefox 113, WebKitGTK 2.40).

const tinted = new Set<string>();
/** Every path passed to `tint` (the scan test holds them to `opaque`). */
export const TINTED: ReadonlySet<string> = tinted;

/** A token's CSS custom property, as a `var()`. */
export function tok(path: string): string {
  if (!SCHEMA_BY_PATH.has(path)) throw new Error(`skin: tok(${JSON.stringify(path)}) — not a token in SKIN_SCHEMA`);
  return `var(${cssName(path)})`;
}

/** An opaque colour token at `pct`% alpha. */
export function tint(path: string, pct: number): string {
  const s = SCHEMA_BY_PATH.get(path);
  if (s === undefined) throw new Error(`skin: tint(${JSON.stringify(path)}) — not a token in SKIN_SCHEMA`);
  if (s.opaque !== true) throw new Error(`skin: tint(${JSON.stringify(path)}) — the token is not opaque, so a tint of it would multiply alphas`);
  if (!(pct >= 0 && pct <= 100)) throw new Error(`skin: tint(${JSON.stringify(path)}, ${pct}) — the percentage is in [0, 100]`);
  tinted.add(path);
  return `color-mix(in srgb, var(${cssName(path)}) ${pct}%, transparent)`;
}
