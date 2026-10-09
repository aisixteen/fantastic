import type { Payload } from "../../lib.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { BACKGROUNDS } from "./backgrounds_agent.ts";

// THE BACKGROUNDS PANEL — the backgrounds agent's own view (one record: the
// agent IS the cell). Top: random · every minute · none · default. Then the
// STOCK list (each with its colour sets; × hides one, `restore stock` brings
// them back) and YOUR list (× deletes), with the hint how an AI fills it. Every
// button is a verb on the agent, so a panel click and an AI's call are the same
// act; the panel redraws on the agent's `updated` events.

const CSS = `
  .bgs { height: 100%; overflow: auto; box-sizing: border-box; padding: 12px 14px; color: #ededeb;
    font: 13px/1.35 system-ui, sans-serif; }
  .bgs h1 { margin: 12px 0 6px; font: 500 11px ui-monospace, SFMono-Regular, Menlo, monospace; letter-spacing: .12em; text-transform: uppercase; color: #8a8884;
    display: flex; justify-content: space-between; align-items: baseline; }
  .bgs h1:first-child { margin-top: 0; }
  .bgs .bar { display: flex; flex-wrap: wrap; gap: 6px; }
  .bgs button { all: unset; cursor: pointer; }
  .bgs .btn { padding: 5px 10px; border-radius: 6px; border: 1px solid #2a2a2c; font: 12px ui-monospace, SFMono-Regular, monospace; color: #cfcdc8; }
  .bgs .btn:hover { border-color: #4a4a4d; background: #141416; }
  .bgs .btn.on { border-color: #7ee2a8; color: #7ee2a8; }
  .bgs .link { font: 11px ui-monospace, monospace; color: #b78cff; text-transform: none; letter-spacing: 0; }
  .bgs .row { display: flex; align-items: center; gap: 8px; padding: 5px 0; border-top: 1px solid #1d1d1f; }
  .bgs .name { flex: 1; min-width: 0; padding: 5px 9px; border-radius: 6px; border: 1px solid #2a2a2c; }
  .bgs .name:hover { border-color: #4a4a4d; background: #141416; }
  .bgs .name.on { border-color: #ededeb55; }
  .bgs .name b { display: block; font-weight: 500; }
  .bgs .name small { display: block; color: #77757a; font: 10.5px ui-monospace, SFMono-Regular, monospace; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .bgs .sets { display: flex; gap: 4px; }
  .bgs .set { display: grid; grid-template-columns: 1fr 1fr; width: 22px; height: 22px; border-radius: 5px; overflow: hidden; outline: 1px solid #ffffff14; outline-offset: 1px; }
  .bgs .set:hover { outline-color: #ffffff55; }
  .bgs .set.on { outline: 2px solid #ededeb; }
  .bgs .x { width: 20px; text-align: center; color: #77757a; font-size: 14px; }
  .bgs .x:hover { color: #ff8a8a; }
  .bgs .hint { margin: 6px 0 2px; padding: 9px 10px; border-radius: 8px; border: 1px dashed #3a3a3e; color: #a9a7a2; font-size: 12px; }
  .bgs .hint b { color: #ededeb; font-weight: 500; }
  .bgs .hint code { font: 11px ui-monospace, SFMono-Regular, monospace; color: #b78cff; }
  .bgs .st { margin-top: 8px; min-height: 16px; color: #77757a; font: 11px ui-monospace, SFMono-Regular, monospace; }
`;

function ensureStyle(): void {
  if (document.getElementById("fantastic-bgs-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-bgs-style";
  s.textContent = CSS;
  document.head.appendChild(s);
}

type Dict = Record<string, unknown>;
interface SetRow { name: string; colors: string[] }
interface Item { name: string; note: string; sets: SetRow[] }

const el = <K extends keyof HTMLElementTagNameMap>(tag: K, cls = "", text = ""): HTMLElementTagNameMap[K] => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text) e.textContent = text;
  return e;
};

export const backgroundsView: ViewBundle = {
  handles: [BACKGROUNDS],
  grants: () => ({}),
  mount(ctx: ViewContext): ViewHandle {
    ensureStyle();
    const root = el("div", "bgs");
    ctx.mount.appendChild(root);
    root.addEventListener("keydown", (e) => e.stopPropagation());
    const st = el("div", "st");
    // the panel's acts are the agent's verbs, sent as the agent itself
    const call = async (p: Dict): Promise<Dict> =>
      (((await ctx.kernel.send([{ target: ctx.selfId, payload: p as Payload }]))[0]?.result ?? {}) as Dict);
    const act = async (p: Dict, doing: string): Promise<void> => {
      st.textContent = doing;
      const r = await call(p);
      st.textContent = typeof r["error"] === "string" ? `✕ ${r["error"]}` : "";
      await render();
    };
    const row = (it: Item, cur: Dict | null, from: "stock" | "user"): HTMLElement => {
      const r = el("div", "row");
      const b = el("button", "name");
      b.append(el("b", "", it.name), el("small", "", it.note || (from === "user" ? "yours" : "")));
      if (cur?.["name"] === it.name) b.classList.add("on");
      b.title = `apply ${it.name}`;
      b.onclick = () => void act({ type: "set", name: it.name }, `applying ${it.name}…`);
      r.append(b);
      if (it.sets.length > 1) {
        const sets = el("div", "sets");
        it.sets.forEach((s, i) => {
          const sb = el("button", "set");
          sb.title = s.name;
          if (cur?.["name"] === it.name && cur?.["set"] === s.name) sb.classList.add("on");
          for (const c of s.colors) {
            const i2 = el("i");
            i2.style.background = c;
            sb.append(i2);
          }
          sb.onclick = () => void act({ type: "set", name: it.name, set: i }, `applying ${it.name} · ${s.name}…`);
          sets.append(sb);
        });
        r.append(sets);
      }
      const x = el("button", "x", "×");
      x.title = from === "stock" ? "hide it (restore stock brings it back)" : "delete it";
      x.onclick = () => void act({ type: "delete", name: it.name }, `removing ${it.name}…`);
      r.append(x);
      return r;
    };
    const render = async (): Promise<void> => {
      const l = await call({ type: "list" });
      const cur = (l["current"] ?? null) as Dict | null;
      const rot = (l["rotate"] ?? {}) as Dict;
      const stock = (l["stock"] ?? []) as Item[];
      const user = (l["user"] ?? []) as Item[];
      const hidden = (l["hidden_stock"] ?? []) as string[];
      root.replaceChildren();
      // the bar
      const bar = el("div", "bar");
      const btn = (label: string, title: string, p: Dict, doing: string, on = false): HTMLElement => {
        const b = el("button", "btn" + (on ? " on" : ""), label);
        b.title = title;
        b.onclick = () => void act(p, doing);
        return b;
      };
      bar.append(
        btn("random bg", "apply a random background", { type: "random" }, "picking…"),
        btn(rot["on"] === true ? "every minute ✓" : "every minute", "a random background every minute", { type: "rotate", on: rot["on"] !== true }, "…", rot["on"] === true),
        btn("none", "clear the background", { type: "none" }, "clearing…", cur === null),
        btn("default", "the bundled background (Isoline · Ember)", { type: "default" }, "…"),
      );
      root.append(bar);
      // stock
      const h1 = el("h1", "", `stock · ${stock.length}`);
      if (hidden.length > 0) {
        const rs = el("button", "link", `restore stock (${hidden.length} hidden)`);
        rs.onclick = () => void act({ type: "restore_stock" }, "restoring…");
        h1.append(rs);
      }
      root.append(h1);
      for (const it of stock) root.append(row(it, cur, "stock"));
      // the user's
      root.append(el("h1", "", `yours · ${user.length}`));
      for (const it of user) root.append(row(it, cur, "user"));
      const hint = el("div", "hint");
      hint.innerHTML =
        "<b>Ask your AI agent to look around and add more backgrounds.</b><br>" +
        "Open an AI cell (double-click → <b>ask</b>) and say e.g. <i>“make three calm backgrounds for this canvas”</i> — " +
        `it adds them here with <code>add</code> on this agent (<code>${ctx.selfId}</code>), and they stay.`;
      root.append(hint, st);
    };
    void render();
    // the agent's state moved (an AI's add, another tab) → draw again
    const off = ctx.kernel.addStateSubscriber((e) => {
      if ((e as Dict)["agent_id"] === ctx.selfId && (e as Dict)["kind"] === "updated") void render();
    });
    return {
      unmount(): void {
        off();
        root.remove();
      },
    };
  },
};
