import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";

// THE MESSAGE STREAM, as a cell — what `fantastic --headless` prints: every
// send on the HOST as `sender → target verb · summary`, live, newest at the
// bottom. It taps the host's state stream over the bridge (`state_subscribe`,
// granted to the canvas leg); nothing on the host is spawned for it. The page
// keeps the last `KEEP` lines only.

type Dict = Record<string, unknown>;

const KEEP = 400;

const CSS = `
  .stream-view { height: 100%; overflow: auto; box-sizing: border-box; padding: 8px 10px;
    font: 11px/1.45 ui-monospace, SFMono-Regular, Menlo, monospace; color: #cfd3e3; background: rgba(8,8,14,0.6); }
  .stream-view .ln { white-space: pre-wrap; word-break: break-word; }
  .stream-view .who { color: #b78cff; }
  .stream-view .verb { color: #7ee2a8; }
  .stream-view .sum { color: rgba(220,230,245,0.55); }
  .stream-view .bad { color: #ff8a8a; }
`;

function ensureStyle(): void {
  if (document.getElementById("fantastic-stream-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-stream-style";
  s.textContent = CSS;
  document.head.appendChild(s);
}

const str = (o: Dict, k: string): string => (typeof o[k] === "string" ? (o[k] as string) : "");

/** One state-stream frame → `[sender, target, verb, summary]`, or null for a
 *  frame that is not a send (the snapshot, drains, row digests). The host's
 *  shape: `state_event {agent_id, kind: "send", sender, summary, payload}`.
 *  Pure, testable. */
export function line(frame: Dict): [string, string, string, string] | null {
  if (str(frame, "type") !== "state_event" || str(frame, "kind") !== "send") return null;
  const target = str(frame, "agent_id");
  if (!target) return null;
  const payload = (frame["payload"] && typeof frame["payload"] === "object" ? frame["payload"] : {}) as Dict;
  const sender = str(frame, "sender") || "kernel";
  return [sender, target, str(payload, "type"), str(frame, "summary")];
}

export const streamView: ViewBundle = {
  handles: ["stream_view.ts"],
  grants: () => ({}),
  mount(ctx: ViewContext): ViewHandle {
    ensureStyle();
    const root = document.createElement("div");
    root.className = "stream-view";
    ctx.mount.appendChild(root);
    const add = (l: [string, string, string, string]): void => {
      const d = document.createElement("div");
      d.className = "ln";
      const [who, to, verb, sum] = l;
      const span = (cls: string, t: string): HTMLElement => {
        const e = document.createElement("span");
        e.className = cls;
        e.textContent = t;
        return e;
      };
      d.append(span("who", who), ` → ${to} `, span("verb", verb), sum ? span("sum", ` · ${sum}`) : "");
      const stick = root.scrollTop + root.clientHeight >= root.scrollHeight - 4;
      root.appendChild(d);
      while (root.childElementCount > KEEP) root.firstElementChild?.remove();
      if (stick) root.scrollTop = root.scrollHeight;
    };
    const off = ctx.kernel.subscribeState((frame) => {
      const l = line(frame);
      if (l !== null) add(l);
    });
    return {
      unmount(): void {
        off();
        root.remove();
      },
    };
  },
};
