import type { Payload } from "../../lib.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { BACKGROUNDS } from "./backgrounds_agent.ts";
import { DEFAULT_THEME } from "../canvas/layout.ts";

// THE CANVAS THEME PANEL (owner 2026-10-12) — two sections: BACKGROUNDS and
// THEMES. Themes: each row uses it on a click; ✎ edits it (name, layout, its
// width trigger), × deletes yours or hides a stock one, `restore stock` brings
// them back, a new one is your AI agent's to add (`add_theme`), the default `canvas` is never deleted; `auto` lets the triggers choose.
//
// THE BACKGROUNDS SECTION — the backgrounds agent's own view (one record: the
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
  .bgs h2 { margin: 18px 0 8px; font: 650 15px system-ui, sans-serif; color: #ededeb; display: flex; align-items: baseline; justify-content: space-between; }
  .bgs h2:first-child { margin-top: 0; }
  .bgs .tform { display: grid; grid-template-columns: 70px minmax(0, 1fr); gap: 6px 10px; align-items: center; padding: 10px; margin: 6px 0; border-radius: 8px; border: 1px solid #3a3a3e; background: #101012; }
  .bgs .tform input, .bgs .tform select { font: 12px ui-monospace, SFMono-Regular, monospace; color: #ededeb; background: #18181b; border: 1px solid #2a2a2c; border-radius: 6px; padding: 5px 7px; min-width: 0; width: 100%; box-sizing: border-box; }
  .bgs .tform .trig select { flex: 1; width: auto; }
  .bgs .tform .trig input { flex: 0 0 72px; }
  .bgs .tform .trig span { flex: 0 0 auto; color: #8a8884; font-size: 12px; }
  .bgs .tform label { color: #8a8884; font-size: 12px; }
  .bgs .tform .trig { display: flex; gap: 6px; align-items: center; }
  .bgs .tform .row2 { grid-column: 1 / -1; display: flex; gap: 8px; justify-content: flex-end; }
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
interface ThemeRow { name: string; layout: "free" | "column"; trigger: { max_width?: number; min_width?: number } | null; note?: string; from: "stock" | "user" }
/** A trigger in words. */
const triggerText = (t: ThemeRow["trigger"]): string =>
  t === null ? "manual" : t.max_width !== undefined && t.min_width !== undefined ? `${t.min_width}–${t.max_width}px wide` : t.max_width !== undefined ? `up to ${t.max_width}px wide` : t.min_width !== undefined ? `from ${t.min_width}px wide` : "manual";

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
    // ONE DRAW AT A TIME: both lists are read first, then the panel is drawn in
    // one go — a redraw overtaken by a newer one (an update event while it
    // waited) draws nothing, or two halves of the panel stacked (two Themes)
    let drawing = 0;
    const render = async (): Promise<void> => {
      const mine = ++drawing;
      const [l, th] = await Promise.all([call({ type: "list" }), call({ type: "themes" })]);
      if (mine !== drawing) return;
      const cur = (l["current"] ?? null) as Dict | null;
      const rot = (l["rotate"] ?? {}) as Dict;
      const stock = (l["stock"] ?? []) as Item[];
      const user = (l["user"] ?? []) as Item[];
      const hidden = (l["hidden_stock"] ?? []) as string[];
      root.replaceChildren(el("h2", "", "Backgrounds"));
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
        "Open an AI cell (double-click → <b>AI agent</b>) and say e.g. <i>“make three calm backgrounds for this canvas”</i> — " +
        `it adds them here with <code>add</code> on this agent (<code>${ctx.selfId}</code>), and they stay.`;
      root.append(hint);
      // ── THEMES ──
      const themes = (th["themes"] ?? []) as ThemeRow[];
      const hiddenT = (th["hidden"] ?? []) as string[];
      const curT = String(th["current"] ?? "auto");
      const h2 = el("h2", "", "Themes");
      const tbar = el("div", "bar");
      tbar.append(
        btn(curT === "auto" ? "auto ✓" : "auto", "let the width triggers choose", { type: "use_theme", name: "auto" }, "…", curT === "auto"),
      );
      if (hiddenT.length > 0) {
        const rs = el("button", "link", `restore stock (${hiddenT.length} hidden)`);
        rs.onclick = () => void act({ type: "restore_themes" }, "restoring…");
        h2.append(rs);
      }
      root.append(h2, tbar);
      for (const t of themes) {
        const r = el("div", "row");
        const b = el("button", "name");
        b.append(el("b", "", t.name), el("small", "", `${t.layout === "column" ? "one column" : "free canvas"} · ${triggerText(t.trigger)}${t.from === "user" ? " · yours" : ""}${t.note ? ` · ${t.note}` : ""}`));
        if (curT === t.name) b.classList.add("on");
        b.title = `use ${t.name}`;
        b.onclick = () => void act({ type: "use_theme", name: t.name }, `switching to ${t.name}…`);
        const ed = el("button", "x", "✎");
        ed.title = t.from === "stock" ? "edit (saves as yours, in place of the stock one)" : "edit";
        ed.onclick = (): void => {
          root.querySelector(".tform")?.remove();
          r.after(themeForm(t));
        };
        const x = el("button", "x", "×");
        x.title = t.from === "stock" ? "hide it (restore stock brings it back)" : "delete it";
        x.onclick = () => void act({ type: "delete_theme", name: t.name }, `removing ${t.name}…`);
        // the default theme stays: no × on it
        if (t.name === DEFAULT_THEME && t.from === "stock") r.append(b, ed);
        else r.append(b, ed, x);
        root.append(r);
      }
      const thint = el("div", "hint");
      thint.innerHTML =
        "<b>To make a new theme, ask your AI agent.</b><br>" +
        "Open an AI cell (double-click → <b>AI agent</b>) and say e.g. <i>“add a theme for wide screens”</i> — " +
        `it adds it here with <code>add_theme</code> on this agent (<code>${ctx.selfId}</code>).`;
      root.append(thint);
      root.append(st);
    };
    // a theme's form: name, layout, its width trigger, a note — saved as yours
    const themeForm = (t: ThemeRow): HTMLElement => {
      const f = el("div", "tform");
      const name = el("input");
      name.value = t.name;
      name.placeholder = "letters, digits, - _";
      const layout = el("select");
      layout.append(Object.assign(el("option"), { value: "free", textContent: "free canvas" }), Object.assign(el("option"), { value: "column", textContent: "one column" }));
      layout.value = t.layout;
      const kind = el("select");
      kind.append(
        Object.assign(el("option"), { value: "manual", textContent: "manual only" }),
        Object.assign(el("option"), { value: "max", textContent: "when the tab is at most" }),
        Object.assign(el("option"), { value: "min", textContent: "when the tab is at least" }),
      );
      const px = el("input");
      px.type = "number";
      const tr = t.trigger;
      kind.value = tr === null ? "manual" : tr.max_width !== undefined ? "max" : "min";
      px.value = String(tr?.max_width ?? tr?.min_width ?? "");
      const note = el("input");
      note.value = t.note ?? "";
      const trig = el("div", "trig");
      trig.append(kind, px, el("span", "", "px wide"));
      const save = el("button", "btn on", "save");
      const cancel = el("button", "btn", "cancel");
      cancel.onclick = () => f.remove();
      save.onclick = (): void => {
        const n = Number(px.value);
        const trigger = kind.value === "manual" || !Number.isFinite(n) || n <= 0 ? null : kind.value === "max" ? { max_width: n } : { min_width: n };
        void act({ type: "add_theme", name: name.value.trim(), layout: layout.value, trigger, note: note.value.trim() }, "saving…");
      };
      const row2 = el("div", "row2");
      row2.append(cancel, save);
      f.append(el("label", "", "name"), name, el("label", "", "layout"), layout, el("label", "", "auto"), trig, el("label", "", "note"), note, row2);
      f.addEventListener("keydown", (e) => e.stopPropagation());
      return f;
    };
    void render();
    // the agent's state moved (an AI's add, another tab) → draw again
    const off = ctx.kernel.addStateSubscriber((e) => {
      // …or its view's theme moved (use_theme, the view's `theme` verb)
      const who = (e as Dict)["agent_id"];
      if ((who === ctx.selfId || who === ctx.kernel.get(ctx.selfId)?.parentId) && (e as Dict)["kind"] === "updated") void render();
    });
    return {
      unmount(): void {
        off();
        root.remove();
      },
    };
  },
};
