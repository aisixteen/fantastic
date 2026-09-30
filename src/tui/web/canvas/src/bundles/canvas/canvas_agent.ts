import { str } from "../../lib.ts";
import type { Handler, Json } from "../../lib.ts";

// THE CANVAS AGENT — the frontend root's own verbs. Its one piece of state is
// the BACKGROUND: a GL source (the same THREE/GLSL text a gl_agent holds) kept
// in the root record's `bg_source`, so it persists through the ws loader and
// rehydrates. The canvas renders it in the shared GL scene, behind every
// member, and swaps it in place when the source changes.
//
// Reached from anywhere in the js kernel — a panel sends
// `{type:"set_bg", source}` to the canvas (its parent: the born-with grant).

export const CANVAS = "canvas.ts";

export const CANVAS_README =
  "Canvas compositor (frontend root). Renders the frontend's own member tree; mounts each member inline via its view bundle, or as an iframe for external content. Not a host client. Owns the BACKGROUND: `set_bg {source}` (a GL source, as a gl_agent's `gl_source`) replaces it, `clear_bg` removes it, `get_bg` returns it; it persists with the canvas record.";

export const canvasAgent: Handler = (id, payload, kernel): Json => {
  const agent = kernel.get(id);
  const bg = (): string => {
    const s = agent?.meta["bg_source"];
    return typeof s === "string" ? s : "";
  };
  switch (str(payload, "type")) {
    case "set_bg": {
      const source = payload["source"];
      if (typeof source !== "string" || source.trim() === "") {
        return { error: "canvas: set_bg needs `source` — a non-empty GL source string", reason: "bad_arg", arg: "source" };
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
      return { error: `canvas: unknown verb '${str(payload, "type")}'`, reason: "unhandled" };
  }
};

/** Register the canvas root's bundle. */
export function registerCanvas(kernel: {
  registerBundle(hm: string, h: Handler): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(CANVAS, canvasAgent);
  kernel.setBundleReadme(CANVAS, CANVAS_README);
}
