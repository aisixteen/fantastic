import { str } from "../../lib.ts";
import { bodyText } from "../body_text.ts";
import { htmlRefusal } from "../html_check.ts";
import { docsOf, entry } from "../declared.ts";
import type { Declared } from "../declared.ts";
import { PANEL_VERBS, panelRelay } from "../panel_relay.ts";
import { grantHint } from "./grant_hint.ts";
import type { FolderBase, Handler, Json, Kernel } from "../../lib.ts";

// html_agent — a JS view-agent holding MUTABLE HTML content. Ported from the
// (deleted) host python bundle: UI-as-record is frontend state now. The body
// lives in the agent's `meta.html`, so `set_html` goes through
// `kernel.updateMeta` → the local state stream → the proxy_loader persists
// it to the host session namespace. The canvas renders the body inline /
// iframed; `reload_html` is the universal reload signal the view subscribes to.

export const HTML_AGENT = "html_agent.ts";

/** PAGE BUNDLES on the html base (`<web root>/bundles/<name>/bundle.yaml`:
 *  `base: html_agent`, `sentence`, `html: |` — the body every instance starts
 *  with): the folder's body, by its name. An instance's own `html` (set_html)
 *  is its own; without one it shows the bundle's. */
const FOLDER_HTML = new Map<string, string>();

/** The body a cell shows: its own `html`, else its page bundle's. */
export function htmlBodyOf(kernel: Kernel, id: string): string {
  const a = kernel.get(id);
  const own = a?.meta["html"];
  if (typeof own === "string") return own;
  return FOLDER_HTML.get(String(a?.record().handler_module ?? "")) ?? "";
}

/** The verbs an html cell answers. */
export const HTML_DECL: Declared = {
  get_html: { doc: "The body (its own, else its bundle's). Returns {id, html}.", args: {} },
  set_html: {
    doc: "This cell's own body from now on; reloads the panel. Returns {ok, bytes, grants_missing?, hint?}.",
    args: { bytes: { type: "bytes", required: true, doc: "The html body, verbatim." } },
  },
  ...PANEL_VERBS,
};

const htmlHandler: Handler = (id, payload, kernel): Json | Promise<Json> => {
  // the panel's own calls, as this agent (`panel_relay.ts`)
  const relayed = panelRelay(kernel, payload);
  if (relayed !== undefined) return relayed;
  const agent = kernel.get(id);
  const body = (): string => htmlBodyOf(kernel, id);
  switch (str(payload, "type")) {
    case "get_html":
      return { id, html: body() };
    case "set_html": {
      // the body is `bytes` (ai_core's convention: a body of text is VERBATIM under `bytes`)
      const raw = bodyText(payload["bytes"]) as string;
      const bad = htmlRefusal("html_agent", raw);
      if (bad !== null) return bad as Json;
      const html = raw;
      kernel.updateMeta(id, { html }); // → updated event → persist
      kernel.emit(id, { type: "reload_html" }); // universal view reload
      return { ok: true, bytes: html.length, ...grantHint(id, html, agent?.meta["can_call"]) } as Json;
    }
    default:
      throw new Error(`html_agent: ${str(payload, "type")} is declared and not handled`);
  }
};

export const htmlAgent: Handler = entry("html_agent", HTML_DECL, htmlHandler);

/** The html BASE: a folder that extends it is an html agent whose body starts
 *  as the folder's `html`. */
export const htmlFolderBase: FolderBase = (name, _folder, spec) => {
  const html = spec["html"];
  if (html !== undefined && typeof html !== "string") return "html: the body, as a string (a literal `|` block)";
  if (typeof html === "string") {
    const bad = htmlRefusal(`bundles/${name}`, html);
    if (bad !== null) return `html: ${String(bad["error"])}`;
  }
  FOLDER_HTML.set(name, typeof html === "string" ? html : "");
  return { handler: htmlAgent, verbs: docsOf(HTML_DECL), readme: String(spec["sentence"]) };
};

export function registerHtmlAgent(kernel: Kernel): void {
  kernel.registerBundle(HTML_AGENT, htmlAgent, docsOf(HTML_DECL), { folderBundle: htmlFolderBase });
}
