import type { Handler } from "../../kernel/agent.ts";
import type { Json } from "../../kernel/json.ts";
import { str } from "../../kernel/json.ts";

// gl_agent — a JS view-agent holding a MUTABLE GL source (THREE/GLSL text).
// Ported from the (deleted) host python bundle: GL content is frontend state
// now. The source lives in the agent's `meta.gl_source`, so a mutation goes
// through `kernel.updateMeta` → the local state stream → the proxy_loader
// persists the record to the host session namespace, like any other record.
// The canvas GL host fetches the source via `get_gl_view` and renders it.

export const GL_AGENT = "gl_agent.ts";

export const glAgent: Handler = (id, payload, kernel): Json => {
  const agent = kernel.get(id);
  switch (str(payload, "type")) {
    case "get_gl_view": {
      // Shape matches the GL host's `GlView` contract (`{source}`); the canvas
      // probes this verb and installs the source into the shared THREE scene.
      const src = agent?.meta["gl_source"];
      return { id, source: typeof src === "string" ? src : "" };
    }
    case "set_gl_source": {
      const source = str(payload, "source");
      kernel.updateMeta(id, { gl_source: source }); // → updated event → persist
      kernel.emit(id, { type: "reload_html" }); // universal view reload
      return { ok: true, bytes: source.length };
    }
    default:
      return { error: `gl_agent: unknown verb '${str(payload, "type")}'` };
  }
};

/** Register the gl_agent bundle so `handler_module: "gl_agent.ts"` records
 *  run locally (and weak-load elsewhere). */
export function registerGlAgent(kernel: {
  registerBundle(hm: string, h: Handler): void;
}): void {
  kernel.registerBundle(GL_AGENT, glAgent);
}
