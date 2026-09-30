import { str } from "../../lib.ts";
import type { Handler, Json } from "../../lib.ts";

// THE BACKGROUND — its own agent on the canvas's surface, a SINGLETON (owner,
// 2026-09-30): exactly one per canvas, a child of the canvas root, created by
// the canvas when missing; it cannot be closed or deleted (`delete_lock: true`
// on its record — the kernel refuses `delete_agent` on it, or on any ancestor
// of it), only updated. Its one piece of state is `bg_source`: a GL source (the
// same THREE/GLSL text a gl_agent holds), rendered behind every member and
// swapped in place when it changes. It persists through the ws loader.

export const BACKGROUND = "background.ts";

/** The background's id on every canvas. */
export const BACKGROUND_ID = "background";

export const BACKGROUND_README =
  "The canvas's BACKGROUND — a singleton: one per canvas, delete-locked (it can only be updated). `set_bg {source}` (a GL source, as a gl_agent's `gl_source`) replaces it, `clear_bg` removes it, `get_bg` returns it; it persists with its record.";

export const backgroundAgent: Handler = (id, payload, kernel): Json => {
  const agent = kernel.get(id);
  const bg = (): string => {
    const s = agent?.meta["bg_source"];
    return typeof s === "string" ? s : "";
  };
  switch (str(payload, "type")) {
    case "set_bg": {
      const source = payload["source"];
      if (typeof source !== "string" || source.trim() === "") {
        return { error: "background: set_bg needs `source` — a non-empty GL source string", reason: "bad_arg", arg: "source" };
      }
      kernel.updateMeta(id, { bg_source: source }); // → updated → persist + re-render
      return { ok: true, bytes: source.length };
    }
    case "clear_bg":
      kernel.updateMeta(id, { bg_source: "" });
      return { ok: true };
    case "get_bg":
      return { source: bg() };
    // The GL host's contract (`GlView`), so the background mounts like any GL view.
    case "get_gl_view":
      return { id, source: bg() };
    case "boot":
    case "shutdown":
      return null;
    default:
      return { error: `background: unknown verb '${str(payload, "type")}'`, reason: "unhandled" };
  }
};

/** Register the background's bundle. */
export function registerBackground(kernel: {
  registerBundle(hm: string, h: Handler): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(BACKGROUND, backgroundAgent);
  kernel.setBundleReadme(BACKGROUND, BACKGROUND_README);
}
