import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import type { Payload } from "../../kernel/json.ts";
import { CONNECTOR_SRC } from "./connector.ts";

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

  mount(ctx: ViewContext): ViewHandle {
    const { kernel, mount, selfId } = ctx;
    const iframe = document.createElement("iframe");
    // null-origin sandbox: scripts run, but the ONLY way out is postMessage to us.
    // EXCEPTION: a TRUSTED first-party panel (`meta.trusted`) renders SAME-ORIGIN, so
    // legacy browser-local coordination — BroadcastChannel between panels, a single
    // AudioContext-unlock gesture shared across them — works as it did before
    // sandboxing. Untrusted html stays null-origin. (allow-same-origin + allow-scripts
    // drops the isolation — only ever for the operator's OWN content.)
    const trusted = kernel.get(selfId)?.meta["trusted"] === true;
    iframe.setAttribute("sandbox", trusted ? "allow-scripts allow-same-origin" : "allow-scripts");
    // Delegate the `autoplay` permission so a panel's AudioContext can resume on a
    // BUS message after ONE top-level user gesture (e.g. a master play) — without a
    // separate click inside every sandboxed panel. Safe: autoplay ≠ same-origin.
    iframe.setAttribute("allow", "autoplay");
    // `meta.html` is the panel BODY (a fragment). Wrap it in a proper document
    // with the connector injected in <head> so it's defined before the body runs.
    const render = (): void => {
      const h = kernel.get(selfId)?.meta["html"];
      const body = typeof h === "string" ? h : "";
      iframe.srcdoc =
        `<!doctype html><html><head><meta charset="utf-8">` +
        `<script>${CONNECTOR_SRC}</script></head><body>${body}</body></html>`;
    };
    render();
    mount.appendChild(iframe);

    const toIframe = (m: Record<string, unknown>): void => {
      iframe.contentWindow?.postMessage({ __ft: true, ...m }, "*");
    };
    const watchOffs = new Map<string, () => void>();

    // Messages the kernel delivers to THIS agent's inbox → forward to the iframe's
    // connector (`onMessage`). `reload_html` (from `set_html`) re-renders instead.
    const offInbox = kernel.onInbox(selfId, (p) => {
      if ((p as Payload)["type"] === "reload_html") render();
      else toIframe({ op: "inbox", payload: p });
    });

    // The relay: connector(postMessage) → the REAL JS kernel. The kernel does the
    // local-vs-host routing — the iframe never knows or cares which.
    const onMsg = (e: MessageEvent): void => {
      if (e.source !== iframe.contentWindow) return; // scope strictly to OUR iframe
      const m = e.data as {
        __ft?: boolean;
        op?: string;
        rid?: string;
        target?: string;
        src?: string;
        payload?: Payload;
      };
      if (m?.__ft !== true) return;
      switch (m.op) {
        case "send":
          void kernel
            .send(m.target ?? "", m.payload ?? {})
            .then((data) => toIframe({ op: "reply", rid: m.rid, data }));
          break;
        case "emit":
          // local frontend agent → local fan-out; host agent → over the bridge.
          if (kernel.get(m.target ?? "") !== undefined) {
            kernel.emit(m.target ?? "", m.payload ?? {});
          } else {
            kernel.emitRemote(m.target ?? "", m.payload ?? {});
          }
          break;
        case "watch": {
          const src = m.src ?? "";
          if (!watchOffs.has(src)) {
            kernel.watch(src, selfId); // host srcs → bridge.watchRemote
            const off = kernel.onInbox(src, (p) => toIframe({ op: "event", src, payload: p }));
            watchOffs.set(src, () => {
              off();
              kernel.unwatch(src, selfId);
            });
          }
          break;
        }
        case "unwatch": {
          const src = m.src ?? "";
          watchOffs.get(src)?.();
          watchOffs.delete(src);
          break;
        }
      }
    };
    window.addEventListener("message", onMsg);

    return {
      unmount(): void {
        window.removeEventListener("message", onMsg);
        offInbox();
        for (const off of watchOffs.values()) off();
        watchOffs.clear();
        iframe.remove();
      },
    };
  },
};
