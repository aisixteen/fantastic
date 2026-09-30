import { Host } from "../../lib.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";

// THE REFLECT CARD — any agent, as a cell: what it says it is (its reflect).
// Opened from the sidebar for an agent no other cell shows. A weak peer: it
// fronts the agent, never owns it (✕ removes the card only).

type Dict = Record<string, unknown>;

const CSS = `
  .reflect-view { padding: 12px 16px; height: 100%; overflow: auto; box-sizing: border-box;
    font: 12px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; color: #d9dcea; }
  .reflect-view h3 { margin: 0 0 4px; font-size: 14px; color: #fff; }
  .reflect-view .hm { color: #b78cff; margin-bottom: 8px; }
  .reflect-view .sentence { color: #aab; margin-bottom: 10px; white-space: pre-wrap; }
  .reflect-view h4 { margin: 12px 0 4px; font-size: 11px; letter-spacing: .1em; color: rgba(220,230,245,0.5); }
  .reflect-view .verb { margin: 2px 0; }
  .reflect-view .verb b { color: #7ee2a8; font-weight: 600; }
  .reflect-view pre { margin: 0; white-space: pre-wrap; word-break: break-word; color: #aab; }
  .reflect-view .err { color: #ff8a8a; }
`;

function ensureStyle(): void {
  if (document.getElementById("fantastic-reflect-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-reflect-style";
  s.textContent = CSS;
  document.head.appendChild(s);
}

const text = (tag: string, cls: string, t: string): HTMLElement => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  e.textContent = t;
  return e;
};

export const reflectView: ViewBundle = {
  handles: ["reflect_view.ts"],
  grants: (backend) => ({ [backend]: ["reflect"] }),
  async mount(ctx: ViewContext): Promise<ViewHandle> {
    ensureStyle();
    const root = document.createElement("div");
    root.className = "reflect-view";
    ctx.mount.appendChild(root);
    const host = new Host(ctx.kernel, ctx.selfId);
    const render = async (): Promise<void> => {
      const r = (await host.call(ctx.backend, { type: "reflect" }).catch((e: Error) => ({ error: e.message }))) as Dict | null;
      root.replaceChildren(text("h3", "", ctx.backend));
      if (r === null || typeof r["error"] === "string") {
        const hint = r !== null && typeof r["hint"] === "string" ? ` — ${r["hint"]}` : "";
        root.append(text("div", "err", `✕ ${r === null ? "no reply" : String(r["error"])}${hint}`));
        return;
      }
      if (typeof r["handler_module"] === "string") root.append(text("div", "hm", r["handler_module"]));
      if (typeof r["sentence"] === "string") root.append(text("div", "sentence", r["sentence"]));
      const verbs = r["verbs"];
      if (verbs && typeof verbs === "object") {
        root.append(text("h4", "", "VERBS"));
        const entries = Array.isArray(verbs) ? verbs.map((v) => [String(v), ""]) : Object.entries(verbs as Dict).map(([k, v]) => [k, typeof v === "string" ? v : ""]);
        for (const [k, v] of entries) {
          const d = document.createElement("div");
          d.className = "verb";
          d.append(text("b", "", k!), v ? ` — ${v}` : "");
          root.append(d);
        }
      }
      const rest = Object.fromEntries(Object.entries(r).filter(([k]) => !["handler_module", "sentence", "verbs"].includes(k)));
      if (Object.keys(rest).length > 0) {
        root.append(text("h4", "", "REFLECT"), text("pre", "", JSON.stringify(rest, null, 2)));
      }
    };
    await render();
    return { unmount: (): void => root.remove() };
  },
};
