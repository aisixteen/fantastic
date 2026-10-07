import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import type { Handler, Json } from "../../lib.ts";
import { str } from "../../lib.ts";
import { panelRelay } from "../panel_relay.ts";
import { mountPanel } from "../html_agent/html_view.ts";
import { hostRecordOf, onViewChange, viewOf } from "../canvas/host_views.ts";

// agent_view — a HOST agent's OWN view (backend-owned views,
// tmp/backend_owned_views.md). NOT page content: the cell holds only layout and
// `backend_id`; the BODY lives on the host agent's record (`view.bytes`, or
// `view.bytes_file` read through the host's file leg) — the agent's own small
// renderer. It runs in the same sandboxed panel as an html cell (`mountPanel`),
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

/** The host agent's view body: inline, or its file through the file leg. */
async function bodyOf(agent: string): Promise<string> {
  const rec = hostRecordOf(agent);
  if (rec === undefined) return `<pre>no host agent ${agent}</pre>`;
  const v = viewOf(rec);
  if (v.bytes !== undefined) return v.bytes;
  if (v.bytesFile !== undefined) {
    const r = await fetch(`/static_files/file/${v.bytesFile.replace(/^\/+/, "")}`).catch(() => null);
    return r?.ok ? r.text() : `<pre>${agent}'s view file ${v.bytesFile} did not load${r ? ` (${r.status})` : ""}</pre>`;
  }
  return `<pre>${agent} declares no view body</pre>`;
}

export const agentViewBundle: ViewBundle = {
  handles: [AGENT_VIEW],
  // the grant comes from the agent's record (`view.calls`), set when placed
  grants: () => ({}),

  mount(ctx: ViewContext): ViewHandle {
    const agent = ctx.backend;
    const panel = mountPanel(ctx, () => bodyOf(agent));
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
