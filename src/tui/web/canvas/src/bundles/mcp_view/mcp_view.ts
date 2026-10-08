import { Host } from "../../lib.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";

// MCP SERVERS on the canvas (owner 2026-10-08): ONE remote MCP server is ONE
// host agent (`mcp_client.tools`, the lib) — its TOOLS ARE ITS VERBS, so an AI
// calls a tool as it calls any agent (`send {type: <tool>, …}`).
//
//   mcp_add_view.ts   the form the double-click places: url + key (optional)
//                     → `add_mcp` on this canvas's node, then the agent's view
//                     replaces the form
//   mcp_view.ts       the agent's view: its url, the server, and the verb list
//                     (`docs of=<agent>`, one signature per tool); live on its
//                     `tools_listed`; ⟳ lists again (`boot`)

export const MCP_VIEW = "mcp_view.ts";
export const MCP_ADD_VIEW = "mcp_add_view.ts";

type Dict = Record<string, unknown>;

export interface McpDeps {
  /** `add_mcp {url, key?}` on this canvas's node → `{id}`, or `{error}` (why not) */
  add(url: string, key: string): Promise<{ id?: string; error?: string }>;
  /** draw host agent `id` where form cell `cell` is, and close the form */
  show(id: string, cell: string): void;
}
let deps: McpDeps | null = null;
export function setMcpDeps(d: McpDeps): void {
  deps = d;
}

const CSS = `
  .mcp-view { padding: 12px 16px; height: 100%; overflow: auto; box-sizing: border-box;
    font: 12px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; color: #d9dcea; }
  .mcp-view .url { color: #fff; font-size: 13px; word-break: break-all; }
  .mcp-view .srv { color: #b78cff; margin: 2px 0 8px; }
  .mcp-view .err { color: #ff8a8a; margin: 4px 0; white-space: pre-wrap; }
  .mcp-view h4 { margin: 10px 0 4px; font-size: 11px; letter-spacing: .1em; color: rgba(220,230,245,0.5); display: flex; justify-content: space-between; }
  .mcp-view h4 button { background: transparent; border: 1px solid rgba(183,140,255,0.4); color: #b78cff; border-radius: 6px; font: inherit; cursor: pointer; padding: 0 6px; }
  .mcp-view .tool { margin: 3px 0; }
  .mcp-view .tool b { color: #7ee2a8; font-weight: 600; }
  .mcp-view .skip { color: rgba(220,230,245,0.45); }
  .mcp-add { padding: 14px 16px; display: flex; flex-direction: column; gap: 8px; font: 13px system-ui, sans-serif; color: #e5e1ee; }
  .mcp-add h3 { margin: 0; font-size: 14px; }
  .mcp-add label { display: flex; flex-direction: column; gap: 3px; font-size: 11px; color: rgba(220,230,245,0.6); }
  .mcp-add input { background: rgba(0,0,0,0.3); border: 1px solid rgba(183,140,255,0.35); color: #fff; font: 13px ui-monospace, monospace; padding: 6px 8px; border-radius: 6px; }
  .mcp-add button { align-self: flex-start; background: rgba(183,140,255,0.25); border: 1px solid rgba(183,140,255,0.5); color: #fff; border-radius: 8px; padding: 6px 14px; cursor: pointer; }
  .mcp-add .note { font-size: 11px; color: rgba(220,230,245,0.5); }
  .mcp-add .err { color: #ff8a8a; font-size: 12px; white-space: pre-wrap; }
`;

function ensureStyle(): void {
  if (document.getElementById("fantastic-mcp-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-mcp-style";
  s.textContent = CSS;
  document.head.appendChild(s);
}

const el = (tag: string, cls: string, t = ""): HTMLElement => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (t) e.textContent = t;
  return e;
};

/** The verbs (tools) a `docs` reply lists, as [name, signature] pairs. */
export function toolsOf(docs: unknown): Array<[string, string]> {
  const v = docs !== null && typeof docs === "object" ? (docs as Dict)["verbs"] : undefined;
  if (Array.isArray(v)) return v.map((x) => [String(x), ""]);
  if (v !== null && typeof v === "object") return Object.entries(v as Dict).map(([k, d]) => [k, typeof d === "string" ? d : ""]);
  return [];
}

export const mcpView: ViewBundle = {
  handles: [MCP_VIEW],
  // reflect (the header), boot (list again), watch (`tools_listed`); the verb
  // list is the kernel's `docs`
  grants: (backend) => ({ [backend]: ["reflect", "boot", "watch"], kernel: ["docs"] }),
  async mount(ctx: ViewContext): Promise<ViewHandle> {
    ensureStyle();
    const root = el("div", "mcp-view");
    ctx.mount.appendChild(root);
    const host = new Host(ctx.kernel, ctx.selfId);
    const render = async (): Promise<void> => {
      const r = (await host.call(ctx.backend, { type: "reflect" }).catch((e: Error) => ({ error: e.message }))) as Dict | null;
      root.replaceChildren();
      if (r === null || typeof r["error"] === "string" && r["url"] === undefined) {
        root.append(el("div", "err", `✕ ${r === null ? "no reply" : String(r["error"])}`));
        return;
      }
      root.append(el("div", "url", String(r["url"] ?? ctx.backend)));
      const srv = (r["server"] ?? {}) as Dict;
      const pv = typeof r["protocol_version"] === "string" ? ` · MCP ${r["protocol_version"]}` : "";
      root.append(el("div", "srv", `${String(srv["name"] ?? "—")} ${String(srv["version"] ?? "")}${pv}`.trim()));
      if (typeof r["last_error"] === "string" && r["last_error"] !== "") root.append(el("div", "err", `✕ ${r["last_error"]}`));
      if (typeof r["instructions"] === "string" && r["instructions"] !== "") root.append(el("div", "skip", r["instructions"]));
      const h = el("h4", "", `TOOLS · ${String(r["tools"] ?? 0)}`);
      const again = el("button", "", "⟳ list again");
      again.title = "ask the server for its tools again (boot)";
      again.addEventListener("click", () => void host.call(ctx.backend, { type: "boot" }).then(() => render()));
      h.append(again);
      root.append(h);
      // the HOST kernel's docs (on the page, "kernel" is the page's own)
      const d = await host.callHost("kernel", { type: "docs", of: ctx.backend } as never).catch(() => null);
      // the agent's own verbs (reflect, boot) are not the server's tools
      for (const [name, sig] of toolsOf(d).filter(([n]) => n !== "reflect" && n !== "boot")) {
        const t = el("div", "tool");
        t.append(el("b", "", name), sig ? ` — ${sig}` : "");
        root.append(t);
      }
      for (const s of Array.isArray(r["skipped"]) ? (r["skipped"] as Dict[]) : []) {
        root.append(el("div", "skip", `skipped ${String(s["name"])}: ${String(s["why"])}`));
      }
    };
    await render();
    // a new listing (boot, or the server's) draws again
    const off = host.on(ctx.backend, "tools_listed", () => void render());
    return {
      unmount(): void {
        off();
        root.remove();
      },
    };
  },
};

export const mcpAddView: ViewBundle = {
  handles: [MCP_ADD_VIEW],
  grants: () => ({}),
  mount(ctx: ViewContext): ViewHandle {
    ensureStyle();
    if (deps === null) throw new Error("mcp_add_view: the canvas is not mounted");
    const d = deps;
    const root = el("form", "mcp-add");
    root.innerHTML = `<h3>MCP server</h3>
      <label>url — the server's MCP endpoint<input name="url" placeholder="https://host/mcp" autocomplete="off"></label>
      <label>key — optional, a static API key (sent as Bearer to that host only)<input name="key" type="password" autocomplete="off"></label>
      <button>add</button><div class="note">its tools become the agent's verbs — an AI calls them like any agent</div><div class="err"></div>`;
    ctx.mount.appendChild(root);
    const err = root.querySelector(".err") as HTMLElement;
    root.addEventListener("keydown", (e) => e.stopPropagation());
    root.addEventListener("submit", (e) => {
      e.preventDefault();
      const url = (root.querySelector("input[name=url]") as HTMLInputElement).value.trim();
      const key = (root.querySelector("input[name=key]") as HTMLInputElement).value.trim();
      if (url === "") {
        err.textContent = "a url is required";
        return;
      }
      err.textContent = "adding…";
      void d.add(url, key).then((r) => {
        if (r.id === undefined) {
          err.textContent = `✕ ${r.error ?? "not added"}`;
          return;
        }
        d.show(r.id, ctx.selfId);
      });
    });
    (root.querySelector("input[name=url]") as HTMLInputElement).focus();
    return { unmount: (): void => root.remove() };
  },
};
