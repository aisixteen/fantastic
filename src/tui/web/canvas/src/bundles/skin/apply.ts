import { backdropNow } from "../shell.ts";
import { DEFAULT_SKINS, stockSkin } from "./stock.ts";
import { compileSkin } from "./tokens.ts";
import type { CompiledSkin } from "./tokens.ts";

// APPLYING A SKIN — the DOM side. A skin is ONE <style id="fantastic-skin">
// holding `:root { --fc-…: …; }`, which every view's CSS reads. The skin is fixed by the backdrop and
// installed once, at boot; JS consumers that need resolved values (xterm's theme
// and font, three.js's clear colour) read `skinNow`.

const STYLE_ID = "fantastic-skin";
let current: { name: string; skin: CompiledSkin } | null = null;

/** Install a compiled skin: the stylesheet and `<html data-skin data-backdrop>`. */
export function installSkin(name: string, skin: CompiledSkin): void {
  current = { name, skin };
  if (typeof document !== "undefined") {
    let style = document.getElementById(STYLE_ID) as HTMLStyleElement | null;
    if (style === null) {
      style = document.createElement("style");
      style.id = STYLE_ID;
      // first in <head>: every view's own <style> comes after and reads its variables
      document.head.prepend(style);
    }
    if (style.textContent !== skin.text) style.textContent = skin.text;
    document.documentElement.dataset["skin"] = name;
    document.documentElement.dataset["backdrop"] = skin.backdrop;
  }
}

/** The stock skin for this tab's backdrop, compiled: the one skin the page shows. */
export function bootSkin(): { name: string; skin: CompiledSkin } {
  const name = DEFAULT_SKINS[backdropNow()];
  const c = compileSkin(stockSkin(name)!.tokens);
  if (!c.ok) throw new Error(`[skin] the stock skin ${name} does not compile: ${c.error}`);
  return { name, skin: c.skin };
}

/** The skin shown now (before any install: the boot skin, not installed). */
export function skinNow(): { name: string; skin: CompiledSkin } {
  return current ?? bootSkin();
}
