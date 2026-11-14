import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import type { Json, Payload } from "../../lib.ts";
import { forgetSize, reportSize, MEASURE_SRC } from "../canvas/fit.ts";
import { mountView } from "../../../../page/view_relay.js";
import { htmlBodyOf } from "./html_agent.ts";

// html_view — renders an html_agent's MUTABLE body (`meta.html`) as a FULLY
// sandboxed `srcdoc` iframe, and wires it to the JS kernel through a per-iframe
// `postMessage` relay. The body uses the injected `fantastic` connector
// (`send`/`emit`/`watch`/`onMessage`) which reaches the JS kernel ONLY; the kernel
// — sole owner of the kernel bridge — routes to other FRONTEND agents locally and
// to HOST agents over the bridge. The iframe has no host URL / WS / same-origin
// access: postMessage to this page is its only channel (no bypass). `set_html`
// emits `reload_html`, which re-renders the iframe in place.

export const htmlView: ViewBundle = {
  // The frontend member IS an `html_agent.ts` agent holding its own content.
  handles: ["html_agent.ts"],
  // Content, not a host client: its reach is whatever its creator wrote.
  grants: () => ({}),

  mount(ctx: ViewContext): ViewHandle {
    // `meta.html` is the panel BODY (a fragment), this agent's own content
    // (a page bundle on the html base starts with its bundle's body)
    return mountPanel(ctx, () => htmlBodyOf(ctx.kernel, ctx.selfId));
  },
};

/** The page URL that loads canvas `id` (`?canvas=<id>`), from the page's own URL. */
export function canvasUrl(pageHref: string, id: string): string {
  const u = new URL(pageHref);
  u.searchParams.set("canvas", id);
  u.hash = "";
  return u.toString();
}

/** THE PANEL: a sandboxed iframe running `body` with the `fantastic` connector,
 *  relayed AS the cell's own agent. Shared by `html_view` (page content) and
 *  `agent_view` (a host agent's own view, `agent_view.ts`). `body` is read at
 *  every render; `reload_html` on the cell's inbox renders again. */
export interface PanelOptions {
  /** the key the panel's size reports are filed under (`fit.ts`); the cell's id by default.
   *  A chat holding several cards gives each its own. */
  sizeKey?: string;
  /** forward the cell's inbox to the panel and re-render on `reload_html` (a cell's own
   *  panel does; a card inside a chat does not: that inbox is the chat's) */
  cellInbox?: boolean;
}

export function mountPanel(ctx: ViewContext, body: () => string | Promise<string>, opts: PanelOptions = {}): ViewHandle & { reload(): void } {
  const { kernel, mount, selfId } = ctx;
  // null-origin sandbox: scripts run, but the ONLY way out is postMessage to us.
  // EXCEPTION: a TRUSTED first-party panel (`meta.trusted`) renders UNSANDBOXED
  // and same-origin — the operator's OWN content. A sandbox that allows both
  // scripts and same-origin isolates nothing (the frame can lift it), and it
  // still blocks what first-party content needs: the browser's PDF viewer is a
  // plugin, sandboxed frames (and every frame nested in one) refuse plugins, so
  // an embedded PDF rendered blank. Untrusted html stays null-origin.
  const trusted = kernel.get(selfId)?.meta["trusted"] === true;
  const sizeKey = opts.sizeKey ?? selfId;

  // THE RELAY IS THE BASE'S ONE MODULE (`web/page/view_relay.js`): it builds the iframe,
  // injects the connector and answers its messages. This page gives it the transport.
  //
  // IT SENDS AS THIS AGENT, NOT AS THE OPERATOR. The iframe holds content the
  // operator did not necessarily write — that is the whole reason it is
  // sandboxed — so its reach is this agent's `can_call`, gated like any other
  // agent's. Relaying as the operator would have made the sandbox cosmetic:
  // null-origin in the browser, unlimited authority in the kernel.
  //
  // Every reach is an explicit grant on this agent's record, and a denial says
  // which end refused and how to open it. The calls run through the agent's
  // own handler (`panel_relay.ts`): identity comes only from dispatch.
  const asSelf = async (verb: string, args: Record<string, unknown>): Promise<Json> =>
    (await kernel.send([{ target: selfId, payload: { type: verb, ...args } as Payload }]))[0]!.result;

  const view = mountView({
    mount,
    html: body,
    trusted,
    extra: MEASURE_SRC,
    transport: {
      send: (target, payload) => asSelf("panel_send", { target, payload }),
      // `emit` AS THIS AGENT (`panel_relay.ts`): gated like a send — the emitter's
      // `can_call` and the receiver's `callable_by` must both name `emit`
      emit: (target, payload) => asSelf("panel_emit", { target, payload }),
      // GATED like a call (#39): a watcher receives `src`'s raw inbound payloads verbatim, so an
      // ungated watch would read everything sent to an agent this iframe may not even call. A
      // denial is reported BACK to the iframe — half-subscribing would look like it worked and
      // deliver nothing, which is the worst of both. Host srcs → bridge.watchRemote.
      watch: async (src, deliver) => {
        const denied = await asSelf("panel_watch", { src, watcher: selfId });
        if (denied !== null) return { denied };
        const off = kernel.onInbox(src, (p) => deliver(p));
        return {
          off: (): void => {
            off();
            kernel.unwatch(src, selfId);
          },
        };
      },
    },
    // messages the kernel delivers to THIS agent's inbox → the iframe's `onMessage`; `reload_html`
    // (from `set_html`) re-renders. A card inside a chat does not take the cell's inbox (the chat's).
    inbox: opts.cellInbox === false ? undefined : (deliver) => kernel.onInbox(selfId, (p) => deliver(p)),
    onMessage: (m: { op?: string; target?: unknown; mw?: unknown; h?: unknown; ox?: unknown }) => {
      if (m.op === "open_canvas") {
        // THE PAGE SWITCHES CANVAS (the same `?canvas=` load the canvas list makes). A
        // panel names a canvas by id; an id this host does not hold fails to load, loudly.
        if (typeof m.target === "string" && m.target !== "") location.href = canvasUrl(location.href, m.target);
      } else if (m.op === "size") {
        // the panel measured its content (`fit.ts`): what `fit` sizes the cell to
        if (typeof m.mw === "number" && typeof m.h === "number") reportSize(sizeKey, { mw: m.mw, h: m.h, ox: m.ox === true });
      }
    },
  });

  return {
    reload: view.reload,
    unmount(): void {
      view.unmount();
      forgetSize(sizeKey);
    },
  };
}
