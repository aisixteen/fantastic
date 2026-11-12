import { str } from "../../lib.ts";
import { bodyText } from "../body_text.ts";
import { docsOf, entry } from "../declared.ts";
import type { Declared } from "../declared.ts";
import { PANEL_VERBS, panelRelay } from "../panel_relay.ts";
import { DEFAULT_BG_SOURCE } from "./default_bg.ts";
import type { Handler, Json } from "../../lib.ts";

// THE BACKGROUND — its own agent on a view's surface, a SINGLETON (owner,
// 2026-09-30/10-04): exactly one per view, a child of the view, with no
// children of its own, created by the page when missing; the root view's
// cannot be closed or deleted (`delete_lock: true` on its record — the kernel
// refuses `delete_agent` on it, or on any ancestor of it), only updated. Its one piece of state is `bg_source`: a GL source (the
// same THREE/GLSL text a gl_agent holds), rendered behind every member and
// swapped in place when it changes. It persists through the ws loader.

export const BACKGROUND = "background.ts";

/** The root view's background id (a nested view's is `<view>_bg`). */
export const BACKGROUND_ID = "background";

/** The background's verbs its view may call — granted BY NAME on both ends
 *  (explicit grants only: no implicit parent reach). */
export const BG_PARENT_VERBS = ["set_bg", "default_bg", "clear_bg", "get_bg", "get_gl_view", "reflect"] as const;

/** The background's own record grant: its view (`$parent`) may call it. */
export function bgCallableBy(): Record<string, string[]> {
  return { $parent: [...BG_PARENT_VERBS] };
}

/** The view's grant onto its background (`$subtree`), merged into `can_call`. */
export function viewToBg(cc: Record<string, unknown> | undefined): Record<string, unknown> {
  const had = cc?.["$subtree"];
  const verbs = had === "*" ? "*" : [...new Set([...(Array.isArray(had) ? (had as string[]) : []), ...BG_PARENT_VERBS])];
  return { ...(cc ?? {}), $subtree: verbs };
}

/** How a GL source runs (the GL host's contract, `gl_host.ts`). */
export const GL_SIGNATURE = ["THREE", "scene", "t", "onFrame", "cleanup"] as const;

/** What the bundle is for (its one short text; how to call it is rendered from the declarations). */
export const BACKGROUND_MAN =
  "A view's BACKGROUND: a singleton, one per view (the root view's is delete-locked: it can only be updated). Its GL source is drawn behind every cell and persists with its record.";

/** The verbs the background answers. */
export const BACKGROUND_DECL: Declared = {
  set_bg: {
    doc: "Replaces the background scene; refused when the source does not compile. Returns {ok, bytes}.",
    args: {
      bytes: {
        type: "bytes",
        required: true,
        doc: `JavaScript, verbatim, run as new Function(${GL_SIGNATURE.map((x) => JSON.stringify(x)).join(", ")}, bytes) in the page's shared three.js scene, behind every cell: THREE is three.js, scene the scene to add to, t the time, onFrame(fn) a per-frame callback fn(t), cleanup(fn) runs on replace. Not CSS.`,
      },
    },
  },
  default_bg: { doc: "Restores the bundled background scene. Returns {ok, bytes}.", args: {} },
  clear_bg: { doc: "Clears the background. Returns {ok}.", args: {} },
  get_bg: { doc: "The background's source. Returns {source}.", args: {} },
  get_gl_view: { doc: "The background's GL view, as the GL host reads it. Returns {id, source}.", args: {} },
  ...PANEL_VERBS,
};

const backgroundHandler: Handler = (id, payload, kernel): Json | Promise<Json> => {
  // its GL source's own emits, as this agent (`panel_relay.ts`)
  const relayed = panelRelay(kernel, payload);
  if (relayed !== undefined) return relayed;
  const agent = kernel.get(id);
  const bg = (): string => {
    const s = agent?.meta["bg_source"];
    return typeof s === "string" ? s : "";
  };
  switch (str(payload, "type")) {
    case "set_bg": {
      // the body is `bytes` (verbatim)
      const source = bodyText(payload["bytes"]) as string;
      // it must COMPILE as the GL host will run it — refused here, with the
      // error, rather than failing later where nobody sees it
      try {
        new Function(...GL_SIGNATURE, source);
      } catch (e) {
        return {
          error: `background: set_bg — the source does not compile: ${(e as Error).message}`,
          reason: "bad_arg",
          arg: "bytes",
          hint: `bytes is JavaScript run as new Function(${GL_SIGNATURE.map((x) => JSON.stringify(x)).join(", ")}, bytes) — reflect the background for an example`,
        };
      }
      kernel.updateMeta(id, { bg_source: source }); // → updated → persist + re-render
      return { ok: true, bytes: source.length };
    }
    case "default_bg":
      kernel.updateMeta(id, { bg_source: DEFAULT_BG_SOURCE });
      return { ok: true, bytes: DEFAULT_BG_SOURCE.length };
    case "clear_bg":
      kernel.updateMeta(id, { bg_source: "" });
      return { ok: true };
    case "get_bg":
      return { source: bg() };
    // The GL host's contract (`GlView`), so the background mounts like any GL view.
    case "get_gl_view":
      return { id, source: bg() };
    default:
      throw new Error(`background: ${str(payload, "type")} is declared and not handled`);
  }
};

export const backgroundAgent: Handler = entry("background", BACKGROUND_DECL, backgroundHandler, ["boot", "shutdown"]);

/** Register the background's bundle. */
export function registerBackground(kernel: {
  registerBundle(hm: string, h: Handler, verbs?: Record<string, string>): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(BACKGROUND, backgroundAgent, docsOf(BACKGROUND_DECL));
  kernel.setBundleReadme(BACKGROUND, BACKGROUND_MAN);
}
