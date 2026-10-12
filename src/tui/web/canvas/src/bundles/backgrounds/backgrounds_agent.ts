import { str } from "../../lib.ts";
import type { Handler, Json, Kernel, Payload } from "../../lib.ts";
import { BACKGROUND, GL_SIGNATURE } from "../canvas/background_agent.ts";
import { STOCK, sourceOf } from "./stock.ts";
import { STOCK_THEMES, themeDefOf, themeNameOk, themesOf } from "../canvas/layout.ts";
import type { ThemeDef } from "../canvas/layout.ts";

// THE CANVAS THEME AGENT (owner 2026-10-12: "rename backgrounds to canvas theme;
// two sections, backgrounds and themes") — its view's BACKGROUNDS (below) and its
// THEMES: the stock ones (`layout.ts` STOCK_THEMES, hideable) and the user's own
// ({name, layout: free | column, trigger: {max_width?, min_width?} | null, note}),
// added and edited like a background. Themes are kept on its record too:
//
//   user_themes    [ThemeDef] — the user's own (a stock name: replaces that one)
//   hidden_themes  [stock names] — stock themes the user removed (`restore_themes`)
//
// and `use_theme` sets its view's `theme` (a name, or auto — by the triggers).
//
// THE BACKGROUNDS (owner 2026-10-09) — a page agent that manages its
// view's background: a STOCK list (`stock.ts`, read-only, hideable), the USER's
// own list (filled by the human or by an AI through `add`), a random pick and an
// automatic one every minute. Its STATE is its record, so it survives reloads
// and restarts:
//
//   user_bgs      [{name, note, bytes} | {name, note, body, sets}] — the user's own
//   hidden_stock  [stock names] — stock entries the user removed (`restore_stock`)
//   current       {name, set} | null — what it applied last
//   rotate        {on, every_s} — the automatic random pick
//
// It applies through its view's `background` agent (`set_bg` / `clear_bg` /
// `default_bg`), granted on both ends when it is made (`grantsFor`).

export const BACKGROUNDS = "backgrounds.ts";

/** What an AI cell (or a panel) calls on it. */
export const BACKGROUNDS_VERBS = ["list", "set", "random", "none", "default", "delete", "restore_stock", "add", "rotate", "themes", "use_theme", "add_theme", "delete_theme", "restore_themes"] as const;

/** The background verbs it calls. */
const BG_CALLS = ["set_bg", "clear_bg", "default_bg", "get_bg"] as const;

/** The automatic pick's default period. */
export const ROTATE_EVERY_S = 60;

export const BACKGROUNDS_README =
  "THE BACKGROUNDS — a view's background manager. STOCK backgrounds (built in; hide one with `delete`, bring all back with `restore_stock`) and the USER's own list. " +
  "`list` → {stock: [{name, note, sets}], user: [{name, note}], hidden_stock, current, rotate}. " +
  "`set {name, set?}` applies one (set: a colour-set name or index; stock only). `random` applies a random visible one. `none` clears the background, `default` restores the bundled one. " +
  "`rotate {on, every_s?}` picks a random one every `every_s` seconds (default 60). " +
  "`delete {name}` removes a user background, or HIDES a stock one. " +
  "`add {name, note?, bytes}` adds a USER background: `bytes` is a GL source in the background's format (JavaScript run as new Function(THREE, scene, t, onFrame, cleanup, bytes) — see the `background` agent's readme). " +
  "Or `add {name, note?, body, sets?}`: a BODY on the stock prelude — it already defines `SET` (4 hex: field, line, mid, accent), `uniforms` (uT time, uC0..uC3 the set's colours, uNoise), GLSL helpers fbm(p), fbm2(p), hair(v,w), hairAt(v,lvl,w), vignette(p), grain(t), hash(p), and backdrop(glsl, {decl?, uniforms?, z?}) — a full-screen plane whose fragment body writes gl_FragColor (vW = world position) — and headline(); read any stock entry (`list`, then `get {name}`) for an example. " +
  "`sets` defaults to the stock colour sets. A source that does not compile is refused, with the error; the same name replaces the old one. `get {name}` → its source. " +
  "THEMES — where the view's cells sit: `themes` → {themes: [{name, layout, trigger, note, from}], hidden, current}. `use_theme {name}` (a name, or auto: the first whose trigger matches the tab's width). " +
  "`add_theme {name, layout: free|column, trigger?: {max_width?, min_width?}, note?}` adds or replaces one of YOURS (a stock name replaces that stock theme). `delete_theme {name}` deletes yours, or hides a stock one; `restore_themes` brings the stock back.";

interface Entry {
  name: string;
  note?: string;
  /** a full GL source (a user background) */
  bytes?: string;
  /** a body on the stock prelude, with its colour sets */
  body?: string;
  sets?: Array<[string, string[]]>;
}

const timers = new Map<string, ReturnType<typeof setInterval>>();

type Dict = Record<string, unknown>;
const arr = <T>(v: unknown): T[] => (Array.isArray(v) ? (v as T[]) : []);

/** The view's background agent (the BACKGROUND child of this agent's parent). */
function backgroundOf(kernel: Kernel, id: string): string | undefined {
  const parent = kernel.get(id)?.parentId;
  const p = parent !== null && parent !== undefined ? kernel.get(parent) : undefined;
  return p === undefined ? undefined : [...p.children.values()].find((a) => a.record().handler_module === BACKGROUND)?.id;
}

/** The grants a new backgrounds agent and its view's background need, both ends. */
export function grantsFor(kernel: Kernel, id: string, parent: string): { canCall: Dict; bg?: { id: string; callableBy: Dict } } {
  const p = kernel.get(parent);
  const bg = p === undefined ? undefined : [...p.children.values()].find((a) => a.record().handler_module === BACKGROUND);
  if (bg === undefined) return { canCall: { $self: "*" } };
  const cb = (bg.meta["callable_by"] ?? {}) as Dict;
  return { canCall: { $self: "*", [bg.id]: [...BG_CALLS] }, bg: { id: bg.id, callableBy: { ...cb, [id]: [...BG_CALLS] } } };
}

function compiles(source: string): string | null {
  try {
    new Function(...GL_SIGNATURE, source);
    return null;
  } catch (e) {
    return (e as Error).message;
  }
}

export const backgroundsAgent: Handler = async (id, payload, kernel): Promise<Json> => {
  const me = kernel.get(id);
  if (me === undefined) return { error: `backgrounds: no agent ${id}`, reason: "not_found" };
  const meta = me.meta;
  const user = (): Entry[] => arr<Entry>(meta["user_bgs"]);
  const hidden = (): string[] => arr<string>(meta["hidden_stock"]);
  const rotate = (): { on: boolean; every_s: number } => {
    const r = (meta["rotate"] ?? {}) as Dict;
    return { on: r["on"] === true, every_s: typeof r["every_s"] === "number" && r["every_s"] >= 5 ? (r["every_s"] as number) : ROTATE_EVERY_S };
  };
  const visibleStock = () => STOCK.filter((s) => !hidden().includes(s.name));
  const bgCall = async (p: Payload): Promise<Dict> => {
    const bg = backgroundOf(kernel, id);
    if (bg === undefined) return { error: "backgrounds: its view has no background agent", reason: "not_found" };
    return ((await kernel.send([{ target: bg, payload: p }]))[0]?.result ?? {}) as Dict;
  };
  const apply = async (name: string, setArg?: unknown): Promise<Json> => {
    const stock = visibleStock().find((s) => s.name === name);
    const own = user().find((u) => u.name === name);
    let source: string;
    let setName: string | null = null;
    if (stock !== undefined || (own !== undefined && own.bytes === undefined)) {
      const e = (stock ?? own) as { body: string; sets?: Array<[string, string[]]> };
      const sets = e.sets && e.sets.length ? e.sets : STOCK[0]!.sets;
      let i = 0;
      if (typeof setArg === "number") i = Math.max(0, Math.min(sets.length - 1, Math.floor(setArg)));
      else if (typeof setArg === "string") i = Math.max(0, sets.findIndex(([n]) => n === setArg));
      setName = sets[i]![0];
      source = sourceOf(e.body, sets[i]![1]);
    } else if (own !== undefined && typeof own.bytes === "string") {
      source = own.bytes;
    } else {
      return { error: `backgrounds: no background ${JSON.stringify(name)}`, reason: "not_found", hint: "`list` names them" };
    }
    const r = await bgCall({ type: "set_bg", bytes: source });
    if (typeof r["error"] === "string") return r as Json;
    kernel.updateMeta(id, { current: { name, set: setName } } as unknown as Payload);
    return { applied: name, set: setName };
  };
  const random = async (): Promise<Json> => {
    const cur = (meta["current"] as Dict | null | undefined)?.["name"];
    const pool = [
      ...visibleStock().map((s) => ({ name: s.name, sets: s.sets.length })),
      ...user().map((u) => ({ name: u.name, sets: u.bytes !== undefined ? 1 : (u.sets?.length ?? STOCK[0]!.sets.length) })),
    ];
    if (pool.length === 0) return { error: "backgrounds: nothing to pick — every stock background is hidden and the user list is empty", reason: "not_found", hint: "`restore_stock`" };
    const others = pool.length > 1 ? pool.filter((p) => p.name !== cur) : pool;
    const pick = others[Math.floor(Math.random() * others.length)]!;
    return apply(pick.name, Math.floor(Math.random() * pick.sets));
  };
  const arm = (): void => {
    const was = timers.get(id);
    if (was !== undefined) clearInterval(was);
    timers.delete(id);
    const r = rotate();
    if (r.on) timers.set(id, setInterval(() => void random(), r.every_s * 1000));
  };

  const userThemes = (): ThemeDef[] => arr<unknown>(meta["user_themes"]).map(themeDefOf).filter((d): d is ThemeDef => d !== null);
  const hiddenThemes = (): string[] => arr<string>(meta["hidden_themes"]);
  const view = me.parentId ?? "";

  switch (str(payload, "type")) {
    case "themes": {
      const all = themesOf(kernel, view);
      const own = new Set(userThemes().map((d) => d.name));
      return {
        themes: all.map((d) => ({ ...d, from: own.has(d.name) ? "user" : "stock" })),
        hidden: hiddenThemes(),
        current: (kernel.get(view)?.meta["theme"] ?? "auto") as Json,
      } as unknown as Json;
    }
    case "use_theme": {
      const name = str(payload, "name") || "auto";
      if (name !== "auto" && !themesOf(kernel, view).some((d) => d.name === name)) return { error: `canvas theme: no theme ${JSON.stringify(name)}`, reason: "not_found", hint: "`themes` names them" };
      kernel.updateMeta(view, { theme: name } as unknown as Payload);
      return { theme: name };
    }
    case "add_theme": {
      const def = themeDefOf({ name: str(payload, "name").trim(), layout: payload["layout"], trigger: payload["trigger"] ?? null, note: str(payload, "note") || undefined });
      if (def === null) return { error: "canvas theme: add_theme needs name (letters, digits, - _) and layout: free | column; trigger, when given, is {max_width?, min_width?}", reason: "bad_arg" };
      kernel.updateMeta(id, { user_themes: [...userThemes().filter((d) => d.name !== def.name), def] } as unknown as Payload);
      return { added: def.name, themes: themesOf(kernel, view).length };
    }
    case "delete_theme": {
      const name = str(payload, "name");
      if (userThemes().some((d) => d.name === name)) {
        kernel.updateMeta(id, { user_themes: userThemes().filter((d) => d.name !== name) } as unknown as Payload);
        return { deleted: name, from: "user" };
      }
      if (STOCK_THEMES.some((d) => d.name === name)) {
        if (!hiddenThemes().includes(name)) kernel.updateMeta(id, { hidden_themes: [...hiddenThemes(), name] } as unknown as Payload);
        return { deleted: name, from: "stock", hint: "`restore_themes` brings the stock back" };
      }
      return { error: `canvas theme: no theme ${JSON.stringify(name)}`, reason: "not_found" };
    }
    case "restore_themes":
      kernel.updateMeta(id, { hidden_themes: [] } as unknown as Payload);
      return { restored: STOCK_THEMES.length };
    case "boot":
      arm(); // a persisted automatic pick runs again
      return null;
    case "shutdown": {
      const t = timers.get(id);
      if (t !== undefined) clearInterval(t);
      timers.delete(id);
      return null;
    }
    case "list":
      return {
        stock: visibleStock().map((s) => ({ name: s.name, note: s.note, sets: s.sets.map(([n, c]) => ({ name: n, colors: c })) })),
        user: user().map((u) => ({ name: u.name, note: u.note ?? "", sets: (u.sets ?? []).map(([n, c]) => ({ name: n, colors: c })) })),
        hidden_stock: hidden(),
        current: (meta["current"] ?? null) as Json,
        rotate: rotate(),
      } as unknown as Json;
    case "get": {
      const name = str(payload, "name");
      const s = STOCK.find((x) => x.name === name);
      const u = user().find((x) => x.name === name);
      if (s !== undefined) return { name, note: s.note, sets: s.sets, body: s.body } as unknown as Json;
      if (u !== undefined) return u as unknown as Json;
      return { error: `backgrounds: no background ${JSON.stringify(name)}`, reason: "not_found" };
    }
    case "set":
      return apply(str(payload, "name"), payload["set"]);
    case "random":
      return random();
    case "none": {
      const r = await bgCall({ type: "clear_bg" });
      if (typeof r["error"] === "string") return r as Json;
      kernel.updateMeta(id, { current: null } as unknown as Payload);
      return { cleared: true };
    }
    case "default": {
      const r = await bgCall({ type: "default_bg" });
      if (typeof r["error"] === "string") return r as Json;
      kernel.updateMeta(id, { current: { name: "Isoline", set: "Ember" } } as unknown as Payload);
      return { applied: "Isoline", set: "Ember" };
    }
    case "rotate": {
      const on = payload["on"] === true;
      const every = typeof payload["every_s"] === "number" && (payload["every_s"] as number) >= 5 ? (payload["every_s"] as number) : rotate().every_s;
      kernel.updateMeta(id, { rotate: { on, every_s: every } } as unknown as Payload);
      arm();
      if (on) await random();
      return { rotate: { on, every_s: every } };
    }
    case "delete": {
      const name = str(payload, "name");
      if (user().some((u) => u.name === name)) {
        kernel.updateMeta(id, { user_bgs: user().filter((u) => u.name !== name) } as unknown as Payload);
        return { deleted: name, from: "user" };
      }
      if (STOCK.some((s) => s.name === name)) {
        if (!hidden().includes(name)) kernel.updateMeta(id, { hidden_stock: [...hidden(), name] } as unknown as Payload);
        return { deleted: name, from: "stock", hint: "`restore_stock` brings the stock back" };
      }
      return { error: `backgrounds: no background ${JSON.stringify(name)}`, reason: "not_found" };
    }
    case "restore_stock":
      kernel.updateMeta(id, { hidden_stock: [] } as unknown as Payload);
      return { restored: STOCK.length };
    case "add": {
      const name = str(payload, "name").trim();
      if (name === "") return { error: "backgrounds: add needs a name", reason: "bad_arg", arg: "name" };
      const note = str(payload, "note");
      const bytes = payload["bytes"];
      const body = payload["body"];
      let entry: Entry;
      if (typeof bytes === "string" && bytes.trim() !== "") {
        const bad = compiles(bytes);
        if (bad !== null) return { error: `backgrounds: add — the source does not compile: ${bad}`, reason: "bad_arg", arg: "bytes" };
        entry = { name, note, bytes };
      } else if (typeof body === "string" && body.trim() !== "") {
        const sets = Array.isArray(payload["sets"]) && (payload["sets"] as unknown[]).length ? (payload["sets"] as Array<[string, string[]]>) : STOCK[0]!.sets;
        const bad = compiles(sourceOf(body, sets[0]![1]));
        if (bad !== null) return { error: `backgrounds: add — the body does not compile on the prelude: ${bad}`, reason: "bad_arg", arg: "body" };
        entry = { name, note, body, sets };
      } else {
        return { error: "backgrounds: add needs `bytes` (a full GL source) or `body` (on the stock prelude)", reason: "bad_arg", arg: "bytes" };
      }
      kernel.updateMeta(id, { user_bgs: [...user().filter((u) => u.name !== name), entry] } as unknown as Payload);
      return { added: name, user: user().length };
    }
    default:
      return {
        error: `backgrounds: unknown verb '${str(payload, "type")}'`,
        reason: "unhandled",
        verb: str(payload, "type"),
        hint: `it answers ${BACKGROUNDS_VERBS.join(", ")}, get`,
      };
  }
};

export function registerBackgrounds(kernel: {
  registerBundle(hm: string, h: Handler, verbs?: Record<string, string>): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(BACKGROUNDS, backgroundsAgent, {
    list: "No args. {stock, user, hidden_stock, current, rotate}.",
    get: "args: name:str. Its source (stock: body + sets; user: bytes or body + sets).",
    set: "args: name:str, set?:str|int (a colour set, stock-style entries). Applies it.",
    random: "No args. Applies a random visible background.",
    none: "No args. Clears the background.",
    default: "No args. The bundled background (Isoline · Ember).",
    rotate: "args: on:bool, every_s?:int (≥ 5; default 60). A random pick every every_s seconds.",
    delete: "args: name:str. Removes a user background, or hides a stock one.",
    restore_stock: "No args. Every hidden stock background back.",
    add: "args: name:str, note?:str, bytes?:str (a full GL source) | body?:str, sets?:[[name, [4 hex]]] (on the stock prelude). Adds a USER background; same name replaces.",
    themes: "No args. {themes: [{name, layout, trigger, note, from: stock|user}], hidden, current}.",
    use_theme: "args: name:str (a theme's name, or auto — the first whose trigger matches the tab's width). Sets the view's theme.",
    add_theme: "args: name:str, layout:free|column, trigger?:{max_width?, min_width?} (px — when auto switches to it), note?:str. Adds or replaces one of YOURS (a stock name replaces that stock theme).",
    delete_theme: "args: name:str. Deletes yours, or hides a stock theme.",
    restore_themes: "No args. Every hidden stock theme back.",
  });
  kernel.setBundleReadme(BACKGROUNDS, BACKGROUNDS_README);
}
