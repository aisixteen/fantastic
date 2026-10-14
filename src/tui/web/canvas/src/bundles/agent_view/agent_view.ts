import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import type { Handler, Json } from "../../lib.ts";
import { str } from "../../lib.ts";
import { panelRelay } from "../panel_relay.ts";
import { mountPanel } from "../html_agent/html_view.ts";
import { Host } from "../../lib.ts";
import { bodyText } from "../body_text.ts";
import { hostRecordOf, onViewChange, viewOf } from "../canvas/host_views.ts";

// agent_view — a HOST agent's OWN view (backend-owned views,
// tmp/backend_owned_views.md). NOT page content: the cell holds only layout and
// `backend_id`; the BODY is the host agent's own small renderer: its BUNDLE's
// view file (`view.bytes_file`, read from `.fantastic/bundles/<bundle>/` through
// the host's `bundles_watch` bridge — the record holds only the name), else the
// record's inline `view.bytes`. It runs in the same sandboxed panel as an html cell (`mountPanel`),
// relayed AS this cell, whose grant onto its agent is derived from the record's
// `view.calls` (`host_views.ts`). A change of the agent's `view` re-renders it.
// `html_agent.ts` stays pure page content.

export const AGENT_VIEW = "agent_view.ts";

/** The cell's own verbs: only the panel's relay (it holds no content). */
export const agentView: Handler = (_id, payload, kernel): Json | Promise<Json> => {
  const relayed = panelRelay(kernel, payload);
  if (relayed !== undefined) return relayed;
  return {
    error: `agent_view: unknown verb '${str(payload, "type")}' — a host agent's view holds no content of its own`,
    reason: "unhandled",
    verb: str(payload, "type"),
    hint: "its body is the HOST agent's `view` (update_agent on that agent, when its bundle lets the view change); its place and size are the view's verbs (move / remove on view_root)",
  };
};

export function registerAgentView(kernel: { registerBundle(hm: string, h: Handler): void }): void {
  kernel.registerBundle(AGENT_VIEW, agentView);
}

/** The host bridge that exposes the host's bundle folders (read-only). */
const BUNDLES_BRIDGE = "bundles_watch";
/** Where the host's bundles live, relative to the project (the bridge's root). */
const HOST_BUNDLES = ".fantastic/bundles";

/** A host file, whole, through the bundles bridge (`read_stream` pages to eof). */
async function readHostFile(host: Host, path: string): Promise<string> {
  let out = "";
  let offset = 0;
  for (;;) {
    const r = (await host.callHost(BUNDLES_BRIDGE, { type: "read_stream", path, offset } as never)) as Record<string, unknown> | null;
    if (typeof r?.["error"] === "string") throw new Error(r["error"]);
    out += bodyText(r?.["bytes"]) ?? "";
    if (r?.["eof"] === true || typeof r?.["next_offset"] !== "number" || r["next_offset"] <= offset) return out;
    offset = r["next_offset"];
  }
}

/** The host agent's view body: its bundle's view file (its `{{agent}}` = this
 *  instance), else the record's inline `bytes`. */
async function bodyOf(host: Host, agent: string): Promise<string> {
  const rec = hostRecordOf(agent);
  if (rec === undefined) return `<pre>no host agent ${agent}</pre>`;
  const v = viewOf(rec);
  if (v.bytesFile !== undefined) {
    const bundle = typeof rec["handler_module"] === "string" ? rec["handler_module"] : "";
    const path = `${HOST_BUNDLES}/${bundle}/${v.bytesFile.replace(/^\/+/, "")}`;
    try {
      return (await readHostFile(host, path)).replaceAll("{{agent}}", agent);
    } catch (e) {
      return `<pre>${agent}'s view file ${path} did not load: ${(e as Error).message}</pre>`;
    }
  }
  if (v.bytes !== undefined) return v.bytes;
  return `<pre>${agent} declares no view body</pre>`;
}

export const agentViewBundle: ViewBundle = {
  handles: [AGENT_VIEW],
  // the grant comes from the agent's record (`view.calls`), set when placed
  grants: () => ({}),

  mount(ctx: ViewContext): ViewHandle {
    const agent = ctx.backend;
    const host = new Host(ctx.kernel, ctx.selfId);
    const panel = mountPanel(ctx, () => bodyOf(host, agent));
    // the agent's `view` changed on the host → render again (the registry says so)
    const off = onViewChange(agent, () => {
      void ctx.kernel.emit(ctx.selfId, { type: "reload_html" } as never);
    });
    return {
      unmount(): void {
        off();
        panel.unmount();
      },
    };
  },
};
