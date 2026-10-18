import type { Kernel } from "../../lib.ts";
import { backdropNow } from "../shell.ts";
import { SKIN_ID, compileNamed, slotsOf, userSkinsOf } from "./skin_agent.ts";
import { DEFAULT_SKINS, stockSkin } from "./stock.ts";
import { compileSkin } from "./tokens.ts";
import type { CompiledSkin } from "./tokens.ts";

// APPLYING A SKIN — the DOM side. A skin is ONE <style id="fantastic-skin">
// holding `:root { --fc-…: …; }`: a switch replaces its text and the browser
// restyles every view; nothing re-renders. JS consumers that need resolved
// values (xterm's theme and font, three.js's clear colour) subscribe `onSkin`.

const STYLE_ID = "fantastic-skin";
type Listener = (name: string, skin: CompiledSkin) => void;
const listeners = new Set<Listener>();
let current: { name: string; skin: CompiledSkin } | null = null;

/** Install a compiled skin: the stylesheet, `<html data-skin data-backdrop>`,
 *  then every `onSkin` listener. */
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
  for (const cb of [...listeners]) {
    try {
      cb(name, skin);
    } catch (e) {
      console.error("[skin] a listener failed:", e);
    }
  }
}

/** The stock skin for this tab's backdrop, compiled (the boot skin). */
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

/** Call `cb` on every skin change; returns the unsubscribe. */
export function onSkin(cb: Listener): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

/** Apply the skin in use from the `skin` agent's record now, and again on every
 *  change of its `skins` or `user_skins`. A skin that cannot apply (an unknown
 *  name, a hand-edited record that no longer compiles) is said loud and toasted;
 *  the page keeps the skin it shows. */
export function watchSkin(kernel: Kernel, toast: (msg: string) => void): void {
  let failed = "";
  const apply = (): void => {
    const me = kernel.get(SKIN_ID);
    if (me === undefined) return;
    const meta = me.meta as Record<string, unknown>;
    const name = slotsOf(meta)[backdropNow()];
    const c = compileNamed(userSkinsOf(meta), name);
    if (!c.ok) {
      const msg = `skin ${name}: ${c.error}`;
      console.error(`[skin] ${msg}`);
      if (msg !== failed) toast(`✕ ${msg}`);
      failed = msg;
      return;
    }
    failed = "";
    const now = current;
    if (now !== null && now.name === name && now.skin.text === c.skin.text && now.skin.backdrop === c.skin.backdrop) return;
    installSkin(name, c.skin);
  };
  apply();
  kernel.addStateSubscriber((e) => {
    if (e.agent_id !== SKIN_ID) return;
    if (e.kind === "added") return apply();
    if (e.kind !== "updated") return;
    const changed = (e as { changed?: unknown }).changed;
    if (!Array.isArray(changed) || changed.includes("skins") || changed.includes("user_skins")) apply();
  });
}
