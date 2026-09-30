import { str } from "../../lib.ts";
import { bodyText } from "../body_text.ts";
import type { Handler, Json } from "../../lib.ts";

// html_agent — a JS view-agent holding MUTABLE HTML content. Ported from the
// (deleted) host python bundle: UI-as-record is frontend state now. The body
// lives in the agent's `meta.html`, so `set_html` goes through
// `kernel.updateMeta` → the local state stream → the proxy_loader persists
// it to the host session namespace. The canvas renders the body inline /
// iframed; `reload_html` is the universal reload signal the view subscribes to.

export const HTML_AGENT = "html_agent.ts";

export const htmlAgent: Handler = (id, payload, kernel): Json => {
  const agent = kernel.get(id);
  const body = (): string => {
    const h = agent?.meta["html"];
    return typeof h === "string" ? h : "";
  };
  switch (str(payload, "type")) {
    case "get_html":
    case "render_html":
      return { id, html: body() };
    case "set_html": {
      // the body is `bytes` (ai_core's convention: a body of text is VERBATIM
      // under `bytes`); `html` is its name here too. A parsed object is not html.
      const raw = bodyText(payload["bytes"] ?? payload["html"]);
      if (raw === null) {
        return { error: "html_agent: set_html needs the html as a string", reason: "bad_arg", arg: "bytes", hint: "send the markup as <bytes>…</bytes> — verbatim, nothing escaped" };
      }
      const html = raw;
      kernel.updateMeta(id, { html }); // → updated event → persist
      kernel.emit(id, { type: "reload_html" }); // universal view reload
      return { ok: true, bytes: html.length };
    }
    default:
      return { error: `html_agent: unknown verb '${str(payload, "type")}'` };
  }
};

/** Register the html_agent bundle so `handler_module: "html_agent.ts"` records
 *  run locally (and weak-load elsewhere). */
export function registerHtmlAgent(kernel: {
  registerBundle(hm: string, h: Handler): void;
}): void {
  kernel.registerBundle(HTML_AGENT, htmlAgent);
}
