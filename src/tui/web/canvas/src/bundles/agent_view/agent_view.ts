import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import type { Handler, Json } from "../../lib.ts";
import { str } from "../../lib.ts";
import { panelRelay } from "../panel_relay.ts";
import { mountPanel } from "../html_agent/html_view.ts";
import type { PanelOptions } from "../html_agent/html_view.ts";
import { onViewChange, viewOf, viewRecordOf } from "../canvas/host_views.ts";

// agent_view — a HOST agent's OWN view (backend-owned views,
// tmp/backend_owned_views.md). NOT page content: the cell holds only layout and
// `backend_id`; the BODY is the host agent's own small renderer: the
// `view.bytes` its record carries (one path: the bytes that arrive — the host
// puts a bundle's view file there). It runs in the same sandboxed panel as an html cell (`mountPanel`),
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

/** The host agent's view body: the `view.bytes` its record carries (the host put
 *  the bundle's view file there, its `{{agent}}` already this instance). */
async function bodyOf(agent: string): Promise<string> {
  const rec = await viewRecordOf(agent);
  if (rec === undefined) return `<pre>no host agent ${agent}</pre>`;
  return viewOf(rec).bytes ?? `<pre>${agent} declares no view body</pre>`;
}

/** A host agent's panel drawn into `ctx.mount`: its body from its record, re-drawn
 *  when its `view` changes. The agent's own cell mounts it (`agentViewBundle`).
 *  `agent` is the host agent shown; calls go out AS `ctx.selfId`, whose `can_call` names
 *  them (`cellGrant`). */
export function mountHostPanel(ctx: ViewContext, agent: string, opts: PanelOptions = {}): ViewHandle {
  // `{{cell}}` in a body is this cell's id: where the panel emits what the canvas should hear
  const panel = mountPanel(ctx, async () => (await bodyOf(agent)).replaceAll("{{cell}}", ctx.selfId), opts);
  // the agent's `view` changed on the host → render again (the registry says so)
  const off = onViewChange(agent, () => panel.reload());
  return {
    unmount(): void {
      off();
      panel.unmount();
    },
  };
}

export const agentViewBundle: ViewBundle = {
  handles: [AGENT_VIEW],
  // the grant comes from the agent's record (`view.calls`), set when placed
  grants: () => ({}),

  mount(ctx: ViewContext): ViewHandle {
    return mountHostPanel(ctx, ctx.backend);
  },
};
