import { str } from "../../lib.ts";
import { bodyText } from "../body_text.ts";
import { docsOf, entry } from "../declared.ts";
import type { Declared } from "../declared.ts";
import { PANEL_VERBS, panelRelay } from "../panel_relay.ts";
import type { Handler, Json } from "../../lib.ts";

// gl_agent — a JS view-agent holding a MUTABLE GL source (THREE/GLSL text).
// Ported from the (deleted) host python bundle: GL content is frontend state
// now. The source lives in the agent's `meta.gl_source`, so a mutation goes
// through `kernel.updateMeta` → the local state stream → the proxy_loader
// persists the record to the host session namespace, like any other record.
// The canvas GL host fetches the source via `get_gl_view` and renders it.

export const GL_AGENT = "gl_agent.ts";

/** The verbs a GL cell answers. */
export const GL_DECL: Declared = {
  get_gl_view: { doc: "This GL cell's source, as the GL host reads it. Returns {id, source}.", args: {} },
  set_gl_source: {
    doc: "Replaces the scene. Returns {ok, bytes}.",
    args: { bytes: { type: "bytes", required: true, doc: "The GL source, verbatim." } },
  },
  ...PANEL_VERBS,
};

const glHandler: Handler = (id, payload, kernel): Json | Promise<Json> => {
  // the GL source's own emits, as this agent (`panel_relay.ts`)
  const relayed = panelRelay(kernel, payload);
  if (relayed !== undefined) return relayed;
  const agent = kernel.get(id);
  switch (str(payload, "type")) {
    case "get_gl_view": {
      // Shape matches the GL host's `GlView` contract (`{source}`); the canvas
      // probes this verb and installs the source into the shared THREE scene.
      const src = agent?.meta["gl_source"];
      return { id, source: typeof src === "string" ? src : "" };
    }
    case "set_gl_source": {
      const source = bodyText(payload["bytes"]) as string;
      kernel.updateMeta(id, { gl_source: source }); // → updated event → persist
      kernel.emit(id, { type: "reload_html" }); // universal view reload
      return { ok: true, bytes: source.length };
    }
    default:
      throw new Error(`gl_agent: ${str(payload, "type")} is declared and not handled`);
  }
};

export const glAgent: Handler = entry("gl_agent", GL_DECL, glHandler);

/** Register the gl_agent bundle so `handler_module: "gl_agent.ts"` records
 *  run locally (and weak-load elsewhere). */
export function registerGlAgent(kernel: {
  registerBundle(hm: string, h: Handler, verbs?: Record<string, string>): void;
}): void {
  kernel.registerBundle(GL_AGENT, glAgent, docsOf(GL_DECL));
}
