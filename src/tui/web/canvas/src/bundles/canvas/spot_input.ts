// THE SPOT INPUT — one text field, opened WHERE you want something (a double
// click on empty canvas, or ⌘K / Ctrl-K at the pointer), macOS-Spotlight style.
//
// Typing a known kind (`terminal`, `html`) creates it at that spot; `@name`
// picks one of the user's AI templates; anything else becomes the FIRST TURN
// of a new AI cell there, on the default template. With no default template the
// top row offers the set-up page instead — nothing is guessed. The field never
// stays: a pick replaces it, and esc / a click away closes it.

export type Pick =
  | { kind: "ai"; template: string | null; text: string }
  | { kind: "terminal" }
  | { kind: "html" }
  | { kind: "setup" };

export interface Presets {
  templates: Array<{ name: string; handler_module: string; model: string }>;
  preferred: string | null;
}

interface Row {
  icon: string;
  label: string;
  hint: string;
  pick: Pick;
}

const CSS = `
  .spot-input { position: fixed; z-index: 1000; width: 460px; max-width: calc(100vw - 32px);
    background: rgba(22, 20, 32, 0.82); backdrop-filter: blur(28px) saturate(180%);
    -webkit-backdrop-filter: blur(28px) saturate(180%); border-radius: 14px;
    box-shadow: inset 0 1px 0 rgba(255,255,255,0.10), 0 0 0 1px rgba(255,255,255,0.12), 0 22px 60px -12px rgba(0,0,0,0.65);
    font: 14px ui-monospace, SFMono-Regular, Menlo, monospace; color: #e5e1ee; overflow: hidden; }
  .spot-input input { width: 100%; box-sizing: border-box; background: transparent; border: 0; outline: none;
    color: #fff; font: 20px system-ui, sans-serif; padding: 14px 16px 12px 44px; }
  .spot-input .spark { position: absolute; left: 16px; top: 15px; font-size: 18px; color: #b78cff; }
  .spot-input ul { list-style: none; margin: 0; padding: 4px 0 6px; border-top: 1px solid rgba(255,255,255,0.08); }
  .spot-input li { display: flex; gap: 10px; padding: 7px 16px; cursor: pointer; }
  .spot-input li .ic { width: 18px; text-align: center; color: #b78cff; }
  .spot-input li .lb { flex: 1; }
  .spot-input li .ht { color: rgba(220,230,245,0.45); font-size: 12px; }
  .spot-input li.sel { background: rgba(183,140,255,0.18); }
  .spot-input .foot { padding: 6px 16px 9px; color: rgba(220,230,245,0.4); font-size: 11px; }
`;

function ensureStyle(): void {
  if (document.getElementById("fantastic-spot-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-spot-style";
  s.textContent = CSS;
  document.head.appendChild(s);
}

/** The rows for what is typed. Pure, so it is testable. */
export function rows(typed: string, presets: Presets | null): Row[] {
  const t = typed.trim();
  const out: Row[] = [];
  const kinds: Array<[string, Pick, string]> = [
    ["terminal", { kind: "terminal" }, "⌨"],
    ["html", { kind: "html" }, "◻"],
  ];
  const at = /^@(\S*)\s*([\s\S]*)$/.exec(t);
  if (at !== null && presets !== null) {
    for (const p of presets.templates.filter((p) => p.name.startsWith(at[1]!))) {
      out.push({ icon: "✦", label: `@${p.name}`, hint: `${p.model} · new AI cell`, pick: { kind: "ai", template: p.name, text: at[2]! } });
    }
    return out;
  }
  const exactKind = kinds.find(([k]) => k === t.toLowerCase());
  if (!exactKind) {
    if (presets?.preferred) {
      out.push({
        icon: "✦",
        label: t ? `ask ${presets.preferred}` : `new AI cell · ${presets.preferred}`,
        hint: "★ default",
        pick: { kind: "ai", template: null, text: t },
      });
    } else {
      out.push({ icon: "✦", label: "configure default AI…", hint: "opens AI set-up", pick: { kind: "setup" } });
    }
  }
  for (const [k, pick, icon] of kinds) {
    if (!t || k.startsWith(t.toLowerCase())) out.push({ icon, label: k, hint: "", pick });
  }
  if (!t && presets) {
    for (const p of presets.templates.filter((p) => p.name !== presets.preferred)) {
      out.push({ icon: "✦", label: `@${p.name}`, hint: p.model, pick: { kind: "ai", template: p.name, text: "" } });
    }
  }
  return out;
}

/** Open the field at a screen point. `onPick` gets the choice; the field is
 *  gone by then. */
export function openSpot(opts: {
  screenX: number;
  screenY: number;
  presets: Presets | null;
  onPick: (p: Pick) => void;
}): () => void {
  ensureStyle();
  document.querySelector(".spot-input")?.remove();
  const box = document.createElement("div");
  box.className = "spot-input";
  const x = Math.max(16, Math.min(opts.screenX - 40, window.innerWidth - 476));
  const y = Math.max(16, Math.min(opts.screenY - 24, window.innerHeight - 320));
  box.style.left = `${x}px`;
  box.style.top = `${y}px`;
  const spark = document.createElement("span");
  spark.className = "spark";
  spark.textContent = "✦";
  const input = document.createElement("input");
  input.placeholder = "ask, or type a kind — terminal, html, @template";
  const list = document.createElement("ul");
  const foot = document.createElement("div");
  foot.className = "foot";
  foot.textContent = "↑↓ pick · ⏎ go · ⇥ complete · esc close";
  box.append(spark, input, list, foot);
  document.body.appendChild(box);

  let sel = 0;
  let current: Row[] = [];
  let done = false;
  const close = (): void => {
    if (done) return;
    done = true;
    box.remove();
    document.removeEventListener("mousedown", away, true);
  };
  const go = (r: Row | undefined): void => {
    if (!r) return;
    close();
    opts.onPick(r.pick);
  };
  const render = (): void => {
    current = rows(input.value, opts.presets);
    sel = Math.min(sel, Math.max(0, current.length - 1));
    list.replaceChildren(
      ...current.map((r, i) => {
        const li = document.createElement("li");
        if (i === sel) li.className = "sel";
        li.innerHTML = `<span class="ic"></span><span class="lb"></span><span class="ht"></span>`;
        (li.children[0] as HTMLElement).textContent = r.icon;
        (li.children[1] as HTMLElement).textContent = r.label;
        (li.children[2] as HTMLElement).textContent = r.hint;
        li.addEventListener("mousedown", (e) => {
          e.preventDefault();
          go(r);
        });
        return li;
      }),
    );
  };
  input.addEventListener("input", () => {
    sel = 0;
    render();
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      sel = Math.min(sel + 1, current.length - 1);
      render();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      sel = Math.max(sel - 1, 0);
      render();
    } else if (e.key === "Tab") {
      e.preventDefault();
      const r = current[sel];
      if (r && r.pick.kind !== "ai" && r.pick.kind !== "setup") input.value = r.label;
      else if (r && r.label.startsWith("@")) input.value = `${r.label} `;
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
