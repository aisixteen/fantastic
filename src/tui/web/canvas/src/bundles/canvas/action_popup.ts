// THE ACTION POPUP — opened WHERE you want something (a double click on empty
// canvas, or ⌘K / Ctrl-K at the pointer), macOS-Launchpad style: a text field on
// top, and below it a GRID of icon tiles in two groups (backend-owned views):
//
//   page    html · stream · demo — page content (FE)
//   host    ask (the default AI) · terminal · agent (set-up) · python agent · your plugins —
//           a HOST agent made, then a cell views it (BE)
//   host · AI     your AI templates (★ the default) — a new AI cell on that one
//   host · chats  your kept dialogs — reopened here
//
// Typing FILTERS the tiles; a kind typed exactly offers only that kind; `@name`
// picks a template with the rest as the first turn; anything else becomes the
// FIRST TURN of a new AI cell on the default template. With no default
// template the first tile opens set-up instead — nothing is guessed. ←↑↓→ move
// across the grid, ⏎ goes, esc / a click away closes; a pick replaces the popup.

export type Pick =
  | { kind: "ai"; template: string | null; text: string }
  | { kind: "terminal" }
  | { kind: "html" }
  | { kind: "stream" }
  | { kind: "setup" }
  | { kind: "demo" }
  | { kind: "sidebar" }
  | { kind: "canvases" }
  | { kind: "chat"; id: string; name: string }
  | { kind: "plugin"; template: string }
  | { kind: "bundle"; starter: string }
  | { kind: "coding"; name: string; path: string };

export interface Presets {
  /** `kind`: "ai" (a chat on it) or "agent" (a plugin: a host agent with its own view) */
  templates: Array<{ name: string; handler_module: string; model: string; kind?: string }>;
  preferred: string | null;
  /** kept dialogs (`backend dialogs`) */
  dialogs: Array<{ id: string; name: string }>;
  /** the known coding agents found on the human's PATH (`user path`) */
  coding?: Array<{ name: string; path: string }>;
}

/** THE CODING AGENTS the menu knows (owner 2026-10-08): LAUNCHERS — a tile
 *  starts the human's own installed program, interactive, in a terminal cell;
 *  nothing is automated, no login is touched. Found by name on `user path`.
 *  Their glyphs are the menu's own outline icons, never a vendor's logo. */
export const CODING_AGENTS: ReadonlyArray<{ name: string; icon: Icon }> = [
  { name: "claude", icon: "claude" },
  { name: "codex", icon: "codex" },
  { name: "aider", icon: "aider" },
  { name: "opencode", icon: "opencode" },
];

export type Section = "page" | "host" | "coding" | "ai" | "chats";
export type Icon = "ask" | "terminal" | "html" | "stream" | "agent" | "template" | "chat" | "demo" | "sidebar" | "canvases" | "claude" | "codex" | "aider" | "opencode";

export interface Tile {
  icon: Icon;
  label: string;
  hint: string;
  pick: Pick;
  section: Section;
}

/** Tiles per row. */
export const COLS = 5;

const KINDS: Array<[string, Pick, Icon, Section]> = [
  ["terminal", { kind: "terminal" }, "terminal", "host"],
  ["agent", { kind: "setup" }, "agent", "host"],
  // a NEW python agent: a project bundle folder from the CLI's starter, one
  // instance placed — edit its folder, the change takes effect at once
  ["python agent", { kind: "bundle", starter: "python_agent" }, "template", "host"],
  ["html", { kind: "html" }, "html", "page"],
  ["stream", { kind: "stream" }, "stream", "page"],
  ["demo", { kind: "demo" }, "demo", "page"],
  ["sidebar", { kind: "sidebar" }, "sidebar", "page"],
  ["canvases", { kind: "canvases" }, "canvases", "page"],
];
/** The groups in DRAWN order — the flat tile order follows it (arrow keys walk it). */
const SECTIONS: Section[] = ["host", "coding", "page", "ai", "chats"];

/** The tiles for what is typed, in order (sections in order). Pure. */
export function tiles(typed: string, presets: Presets | null): Tile[] {
  const t = typed.trim();
  const low = t.toLowerCase();
  const out: Tile[] = [];
  const all = presets?.templates ?? [];
  const templates = all.filter((p) => p.kind !== "agent");
  const plugins = all.filter((p) => p.kind === "agent");
  const dialogs = presets?.dialogs ?? [];
  const at = /^@(\S*)\s*([\s\S]*)$/.exec(t);
  if (at !== null) {
    for (const p of templates.filter((p) => p.name.startsWith(at[1]!))) {
      out.push({ icon: "template", label: `@${p.name}`, hint: p.model, pick: { kind: "ai", template: p.name, text: at[2]! }, section: "ai" });
    }
    return out;
  }
  const exact = KINDS.find(([k]) => k === low);
  if (exact) return [{ icon: exact[2], label: exact[0], hint: "", pick: exact[1], section: exact[3] }];
  // the first tile: the default AI (free text → its first turn), or set-up
  out.push(
    presets?.preferred
      ? { icon: "ask", label: t ? `ask ${presets.preferred}` : "ask", hint: `★ ${presets.preferred}`, pick: { kind: "ai", template: null, text: t }, section: "host" }
      : { icon: "agent", label: "configure AI…", hint: "no default yet", pick: { kind: "setup" }, section: "host" },
  );
  for (const [k, pick, icon, section] of KINDS) {
    if (!t || k.startsWith(low)) out.push({ icon, label: k, hint: "", pick, section });
  }
  for (const p of plugins) {
    if (t && !p.name.toLowerCase().startsWith(low)) continue;
    out.push({ icon: "agent", label: p.name, hint: "plugin", pick: { kind: "plugin", template: p.name }, section: "host" });
  }
  for (const c of presets?.coding ?? []) {
    if (t && !c.name.startsWith(low)) continue;
    const icon = CODING_AGENTS.find((a) => a.name === c.name)?.icon ?? "terminal";
    out.push({ icon, label: c.name, hint: c.path, pick: { kind: "coding", name: c.name, path: c.path }, section: "coding" });
  }
  for (const p of templates) {
    if (t && !p.name.toLowerCase().startsWith(low)) continue;
    const star = p.name === presets?.preferred ? " ★" : "";
    out.push({ icon: "template", label: `@${p.name}${star}`, hint: p.model, pick: { kind: "ai", template: p.name, text: "" }, section: "ai" });
  }
  for (const d of dialogs) {
    if (t && !d.name.toLowerCase().includes(low)) continue;
    out.push({ icon: "chat", label: d.name, hint: "kept chat", pick: { kind: "chat", id: d.id, name: d.name }, section: "chats" });
  }
  // stable by group: the first tile stays the default AI (free text + ⏎ asks it)
  return SECTIONS.flatMap((sec) => out.filter((x) => x.section === sec));
}

/** Arrow-key move across the grid of `n` tiles: ±1 across, ±COLS down/up. */
export function move(sel: number, key: string, n: number): number {
  if (n === 0) return 0;
  const d = key === "ArrowRight" ? 1 : key === "ArrowLeft" ? -1 : key === "ArrowDown" ? COLS : key === "ArrowUp" ? -COLS : 0;
  return Math.max(0, Math.min(n - 1, sel + d));
}

// ── icons: inline SVG, 24×24, stroke = currentColor ─────────────────────────
const PATH: Record<Icon, string> = {
  ask: '<path d="M12 3l1.8 4.7L18.5 9.5l-4.7 1.8L12 16l-1.8-4.7L5.5 9.5l4.7-1.8z"/><path d="M18 15l.8 2.2L21 18l-2.2.8L18 21l-.8-2.2L15 18l2.2-.8z"/>',
  terminal: '<rect x="3" y="4" width="18" height="16" rx="2.5"/><path d="M7 9l3 3-3 3M12.5 15H17"/>',
  html: '<rect x="3" y="4" width="18" height="16" rx="2.5"/><path d="M3 8.5h18M6 6.3h.01M8.5 6.3h.01"/>',
  stream: '<path d="M4 6h16M4 10h11M4 14h14M4 18h8"/>',
  agent: '<circle cx="12" cy="12" r="3"/><path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3M5.3 5.3l2.1 2.1M16.6 16.6l2.1 2.1M5.3 18.7l2.1-2.1M16.6 7.4l2.1-2.1"/>',
  template: '<path d="M12 3l1.8 4.7L18.5 9.5l-4.7 1.8L12 16l-1.8-4.7L5.5 9.5l4.7-1.8z"/><rect x="15" y="15" width="6" height="6" rx="1.5"/>',
  demo: '<circle cx="12" cy="12" r="9"/><path d="M10 8.5v7l6-3.5z"/>',
  sidebar: '<rect x="4" y="4" width="16" height="16" rx="2"/><path d="M9 4v16M12 9h5M12 13h5"/>',
  // the coding agents: plain outline marks of the menu's own, no vendor logo
  claude: '<path d="M12 3v18M3 12h18M5.6 5.6l12.8 12.8M18.4 5.6L5.6 18.4"/>',
  codex: '<path d="M12 3l8 4.5v9L12 21l-8-4.5v-9z"/><path d="M9 10l-2 2 2 2M15 10l2 2-2 2"/>',
  aider: '<rect x="3" y="6" width="18" height="12" rx="2"/><path d="M8 10v4M16 10v4M11 12h2"/>',
  opencode: '<path d="M8 7l-5 5 5 5M16 7l5 5-5 5M14 4l-4 16"/>',
  canvases: '<rect x="3" y="5" width="8" height="6" rx="1"/><rect x="13" y="5" width="8" height="6" rx="1"/><rect x="3" y="13" width="8" height="6" rx="1"/><rect x="13" y="13" width="8" height="6" rx="1"/>',
  chat: '<path d="M4 5.5A2.5 2.5 0 0 1 6.5 3h11A2.5 2.5 0 0 1 20 5.5v8a2.5 2.5 0 0 1-2.5 2.5H10l-4.5 4v-4H6.5A2.5 2.5 0 0 1 4 13.5z"/>',
};
export function iconSvg(icon: Icon): string {
  return `<svg viewBox="0 0 24 24" width="26" height="26" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">${PATH[icon]}</svg>`;
}

const SECTION_TITLE: Record<Section, string> = { page: "page", host: "host", coding: "host · coding agents", ai: "host · AI", chats: "host · chats" };

const CSS = `
  .action-popup { position: fixed; z-index: 1000; width: 520px; max-width: calc(100vw - 32px);
    background: rgba(22, 20, 32, 0.84); backdrop-filter: blur(28px) saturate(180%);
    -webkit-backdrop-filter: blur(28px) saturate(180%); border-radius: 16px;
    box-shadow: inset 0 1px 0 rgba(255,255,255,0.10), 0 0 0 1px rgba(255,255,255,0.12), 0 22px 60px -12px rgba(0,0,0,0.65);
    font: 13px system-ui, sans-serif; color: #e5e1ee; overflow: hidden; }
  .action-popup input { width: 100%; box-sizing: border-box; background: transparent; border: 0; outline: none;
    color: #fff; font: 20px system-ui, sans-serif; padding: 14px 16px 12px 46px; }
  .action-popup .spark { position: absolute; left: 14px; top: 13px; color: #b78cff; }
  .action-popup .grid { border-top: 1px solid rgba(255,255,255,0.08); padding: 6px 10px 8px; max-height: 360px; overflow-y: auto; }
  .action-popup .sect { font-size: 10px; letter-spacing: .12em; text-transform: uppercase; color: rgba(220,230,245,0.42); padding: 8px 6px 4px; }
  .action-popup .row { display: grid; grid-template-columns: repeat(${COLS}, 1fr); gap: 4px; }
  .action-popup .tile { display: flex; flex-direction: column; align-items: center; gap: 5px; padding: 10px 4px 8px;
    border-radius: 12px; cursor: pointer; color: #d9d4e6; min-width: 0; }
  .action-popup .tile .ic { color: #b78cff; display: flex; }
  .action-popup .tile .lb { font-size: 12px; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .action-popup .tile .ht { font-size: 10px; color: rgba(220,230,245,0.42); max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .action-popup .tile.sel { background: rgba(183,140,255,0.22); box-shadow: inset 0 0 0 1px rgba(183,140,255,0.45); }
  .action-popup .tile[data-section="chats"] .ic { color: #7ee2a8; }
  .action-popup .foot { padding: 6px 16px 9px; color: rgba(220,230,245,0.4); font-size: 11px; border-top: 1px solid rgba(255,255,255,0.06); }
`;

function ensureStyle(): void {
  if (document.getElementById("fantastic-action-popup-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-action-popup-style";
  s.textContent = CSS;
  document.head.appendChild(s);
}

/** Open the popup at a screen point. `onPick` gets the choice; the popup is
 *  gone by then. Returns a closer. */
export function openActionPopup(opts: {
  screenX: number;
  screenY: number;
  presets: Presets | null;
  onPick: (p: Pick) => void;
}): () => void {
  ensureStyle();
  document.querySelector(".action-popup")?.remove();
  const box = document.createElement("div");
  box.className = "action-popup";
  box.style.left = `${Math.max(16, Math.min(opts.screenX - 40, window.innerWidth - 536))}px`;
  box.style.top = `${Math.max(16, Math.min(opts.screenY - 24, window.innerHeight - 480))}px`;
  const spark = document.createElement("span");
  spark.className = "spark";
  spark.innerHTML = iconSvg("ask");
  const input = document.createElement("input");
  input.placeholder = "ask, or search…";
  const grid = document.createElement("div");
  grid.className = "grid";
  const foot = document.createElement("div");
  foot.className = "foot";
  foot.textContent = "←↑↓→ pick · ⏎ go · esc close";
  box.append(spark, input, grid, foot);
  document.body.appendChild(box);

  let sel = 0;
  let current: Tile[] = [];
  let done = false;
  const close = (): void => {
    if (done) return;
    done = true;
    box.remove();
    document.removeEventListener("mousedown", away, true);
  };
  const go = (t: Tile | undefined): void => {
    if (!t) return;
    close();
    opts.onPick(t.pick);
  };
  const render = (): void => {
    current = tiles(input.value, opts.presets);
    sel = Math.min(sel, Math.max(0, current.length - 1));
    const parts: HTMLElement[] = [];
    let i = 0;
    for (const section of SECTIONS) {
      const these = current.filter((t) => t.section === section);
      if (these.length === 0) continue;
      const h = document.createElement("div");
      h.className = "sect";
      h.textContent = SECTION_TITLE[section];
      const row = document.createElement("div");
      row.className = "row";
      for (const t of these) {
        const idx = i++;
        const el = document.createElement("div");
        el.className = "tile" + (idx === sel ? " sel" : "");
        el.dataset["section"] = section;
        el.dataset["kind"] = t.pick.kind;
        el.innerHTML = `<span class="ic">${iconSvg(t.icon)}</span><span class="lb"></span><span class="ht"></span>`;
        (el.children[1] as HTMLElement).textContent = t.label;
        (el.children[2] as HTMLElement).textContent = t.hint;
        el.title = t.hint ? `${t.label} — ${t.hint}` : t.label;
        el.addEventListener("mousedown", (e) => {
          e.preventDefault();
          go(t);
        });
        row.appendChild(el);
      }
      parts.push(h, row);
    }
    grid.replaceChildren(...parts);
    grid.querySelector(".tile.sel")?.scrollIntoView({ block: "nearest" });
  };
  input.addEventListener("input", () => {
    sel = 0;
    render();
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp" || ((e.key === "ArrowLeft" || e.key === "ArrowRight") && input.value === "")) {
      // ← → move tiles only while the field is empty (else they edit the text)
      e.preventDefault();
      sel = move(sel, e.key, current.length);
      render();
    } else if (e.key === "Enter") {
      e.preventDefault();
      go(current[sel]);
    }
  });
  const away = (e: MouseEvent): void => {
    if (!box.contains(e.target as Node)) close();
  };
  document.addEventListener("mousedown", away, true);
  render();
  input.focus();
  return close;
}
