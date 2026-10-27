import { Host } from "../lib.ts";
import type { ViewContext, ViewHandle } from "./view.ts";
import { tint, tok } from "./skin/css.ts";
import type { Dict, Row } from "./agent_models/rows.ts";

// THE SHARED BODY of the read-only agent views (resources, tasks, store,
// scheduler, cabinets, credentials): a header with ⟳, rows drawn as TEXT (never
// markup), and the agent's events as the live path — an event on the agent
// reads it again. A view supplies `load` (the reads it may make: its grant) and
// `shape` (the pure model: replies → rows).

const CSS = `
  .ro-view { padding: 10px 14px; height: 100%; overflow: auto; box-sizing: border-box;
    font: 12px/1.5 ${tok("font.family.mono")}; color: ${tok("color.text.cool")}; }
  .ro-view .bar { display: flex; justify-content: flex-end; margin-bottom: 4px; }
  .ro-view button { background: transparent; border: 1px solid ${tint("color.accent.base", 40)}; color: ${tok("color.accent.base")}; border-radius: ${tok("radius.control")}; font: inherit; cursor: pointer; padding: 0 6px; }
  .ro-view .ctl { display: flex; gap: 6px; margin: 4px 0 8px; }
  .ro-view input { flex: 1; background: ${tint("color.shade", 30)}; border: 1px solid ${tint("color.accent.base", 35)}; color: ${tok("color.text.bright")}; font: inherit; padding: 3px 6px; border-radius: ${tok("radius.control")}; }
  .ro-view .head { margin: 10px 0 4px; font-size: 11px; letter-spacing: .1em; color: ${tint("color.text.cool", 50)}; }
  .ro-view .line { color: ${tok("color.text.bright")}; white-space: pre-wrap; word-break: break-word; }
  .ro-view .line[data-id] { cursor: pointer; }
  .ro-view .dim { color: ${tint("color.text.cool", 55)}; white-space: pre-wrap; word-break: break-word; }
  .ro-view .err { color: ${tok("color.status.error")}; white-space: pre-wrap; }
`;

function ensureStyle(): void {
  if (document.getElementById("fantastic-ro-view-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-ro-view-style";
  s.textContent = CSS;
  document.head.appendChild(s);
}

export type Call = (payload: Dict) => Promise<unknown>;

export interface ReadViewSpec<T> {
  /** the CSS class of the body, and the view's name */
  name: string;
  /** read the agent: only the verbs the view's grant names */
  load(call: Call, ctx: ViewContext): Promise<T>;
  /** the pure model: what was read → rows */
  shape(data: T): Row[];
  /** extra controls under the header (a search box); `redraw` shapes again, `call` reads */
  controls?(box: HTMLElement, tools: { call: Call; redraw(): void }): void;
  /** a click on a row that carries an id */
  onRow?(id: string, redraw: () => void): void;
}

const el = (tag: string, cls: string, t = ""): HTMLElement => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (t) e.textContent = t;
  return e;
};

export async function mountReadView<T>(ctx: ViewContext, spec: ReadViewSpec<T>): Promise<ViewHandle> {
  ensureStyle();
  const root = el("div", `ro-view ${spec.name}`);
  ctx.mount.appendChild(root);
  const host = new Host(ctx.kernel, ctx.selfId);
  const call: Call = (p) => host.call(ctx.backend, p as never).catch((e: Error) => ({ error: e.message }));
  const bar = el("div", "bar");
  const again = el("button", "refresh", "⟳ refresh");
  again.title = "read it again";
  bar.appendChild(again);
  const ctl = el("div", "ctl");
  const body = el("div", "body");
  for (const c of [bar, ctl, body]) root.appendChild(c);
  let data: T | undefined;
  const redraw = (): void => {
    if (data === undefined) return;
    body.replaceChildren();
    for (const r of spec.shape(data)) {
      const row = el("div", r.kind, `${"  ".repeat(r.depth ?? 0)}${r.text}`);
      if (r.id !== undefined) {
        row.setAttribute("data-id", r.id);
        row.addEventListener("click", () => spec.onRow?.(r.id!, redraw));
      }
      body.appendChild(row);
    }
  };
  const refresh = async (): Promise<void> => {
    data = await spec.load(call, ctx);
    redraw();
  };
  again.addEventListener("click", () => void refresh());
  spec.controls?.(ctl, { call, redraw });
  await refresh();
  // LIVE: an event on the agent reads it again
  const off = ctx.kernel.onInbox(ctx.backend, () => void refresh());
  ctx.kernel.watch(ctx.backend, ctx.selfId);
  return {
    unmount(): void {
      off();
      host.unwatch(ctx.backend);
      root.remove();
    },
  };
}
