import { Agent, str } from "../../lib.ts";
import { mcpView } from "../mcp_view/mcp_view.ts";
import type { Handler, Json, Kernel, Payload } from "../../lib.ts";
import { terminalView } from "../terminal_view/terminal_view.ts";
import { RENDERER_CHOICES, resolveRenderer } from "../terminal_view/renderer.ts";
import { readShell } from "../shell.ts";
import { bodyText } from "../body_text.ts";
import { htmlRefusal } from "../html_check.ts";
import { aiView } from "../ai_view/ai_view.ts";
import { fileGrant, isRefusal, kernelBridge, openViewAgent, said } from "../files/file_bridge.ts";
import { agentIndex } from "../files/files_deps.ts";
import { filesAgent } from "../files/files_controller.ts";
import { isRefusalOf, resolveTarget } from "../files/drag.ts";
import type { Look, Opened, Refusal, Target } from "../files/drag.ts";
import { nameOf, norm, parentOf, wire } from "../files/paths.ts";
import { grantHint } from "../html_agent/grant_hint.ts";
import { reflectView } from "../reflect_view/reflect_view.ts";
import { cellGrant, hostRecordFresh, viewOf } from "./host_views.ts";
import type { Rec } from "./host_views.ts";
import { hiddenOf, setHidden, setRect, tabWidth, themeOf, themesOf } from "./layout.ts";
import { isCellHandler } from "./cell_menu.ts";
import { SIDEBAR_ID } from "./tree_sidebar.ts";
import { chromeOf, FIT_MAX, fitSize, nextSize, sizeOf } from "./fit.ts";
import type { Rect } from "./layout.ts";

/** The cell meta that draws host agent `agent` by its own view (`host_views.ts`). */
export function hostViewCell(agent: string, rec: Rec): { handler: string; meta: Record<string, unknown> } {
  const v = viewOf(rec);
  const meta: Record<string, unknown> = { backend_id: agent, owns_backend: false };
  // the renderer's OWN grant onto its agent (terminal, chat and the stub declare
  // theirs); an html view's is its record's `calls`
  meta["can_call"] =
    v.kind === "terminal" ? { ...terminalView.grants(agent) }
    : v.kind === "chat" ? { ...aiView.grants(agent) }
    : v.kind === "stub" ? { ...reflectView.grants(agent) }
    : v.kind === "mcp" ? { ...mcpView.grants(agent) }
    : cellGrant(agent, v);
  // the BODY stays on the host record (`agent_view` reads it there): the cell
  // holds layout and the grant only
  return { handler: v.handler, meta };
}

// THE CANVAS AND ITS VIEW (owner, 2026-10-04: one canvas, ONE view for now —
// more views are on hold).
//
//   canvas            canvas.ts   the page's root: holds the view
//   └── view_root     view.ts     THE VIEW: its cells and its background
//       └── background · <its cells>
//
// A VIEW's own verbs are the CONTROL PLANE over its own cells (owner
// 2026-09-30: an AI cell is the control plane) — `place`, `move`, `update`,
// `wire`, `remove`, `theme`, `terminal_renderer`. The page kernel has no tree-CRUD verbs by design;
// the view, which owns its members, answers these. Its background is its own
// singleton child agent (`background_agent.ts`); where its cells sit is its
// per-theme layout children (`layout.ts`).

/** The page's root agent. */
export const CANVAS = "canvas.ts";
/** A view: a container of cells. */
export const VIEW = "view.ts";
/** The root view's id: every page has exactly one, under the root. */
export const ROOT_VIEW = "view_root";

/** The kinds `place` makes: page-side content cells, and a view of an existing
 *  host agent (a terminal onto an exec agent, a chat onto an AI) — weak peers. */
export const PLACEABLE: Record<string, string> = {
  html: "html_agent.ts",
  gl: "gl_agent.ts",
  terminal: "terminal_view.ts",
  chat: "ai_view.ts",
};

/** Fields `place` / `update` may set — a cell's grants included (the view is
 *  its parent); never identity, placement in the tree or a view's binding. */
export const SETTABLE = ["display_name", "html", "gl_source", "trusted", "readme", "can_call", "callable_by"] as const;

/** A cell's GEOMETRY: not on its record — in the view's layout for the theme in
 *  use (`layout.ts`). `place`, `move` and `update` take these and write them there. */
export const GEOMETRY = ["x", "y", "width", "height"] as const;

/** Every view verb, for the grants that admit a caller. */
export const VIEW_VERBS = ["place", "move", "fit", "update", "wire", "remove", "hide", "show", "open", "tree", "theme", "terminal_renderer", "reflect"] as const;

const num = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

/** The geometry fields of a call, as a rect patch. */
const geometryOf = (p: Record<string, unknown>): Partial<Rect> => {
  const m: Partial<Rect> = {};
  for (const k of GEOMETRY) if (num(p[k])) m[k] = p[k] as number;
  return m;
};

/** A cell's BODY from a call: `bytes` (ai_core's verbatim convention), or the
 *  kind's own field (`html` / `gl_source`). undefined = none given; a refusal
 *  when it is not a string (the grammar parsed markup's tags as arguments). */
function bodyOf(kind: string, payload: Payload): string | undefined | Record<string, unknown> {
  const field = kind === "gl" ? "gl_source" : "html";
  const given = payload["bytes"] ?? payload[field];
  if (given === undefined) return undefined;
  const raw = bodyText(given);
  if (raw !== null) {
    // an html body must BE html (a placeholder, a truncated script: refused)
    if (field === "html") return htmlRefusal("view", raw) ?? raw;
    return raw;
  }
  return { error: `view: the ${field} must be a string`, reason: "bad_arg", arg: "bytes", hint: "send the markup as <bytes>…</bytes> — verbatim, nothing escaped" };
}


/** How long `fit` waits for a panel's measure: its first, and the one after its
 *  new width (a paragraph wraps differently). */
const FIT_WAIT_MS = 3000;

/** `fit`: the width the content wants, then — the panel measured again at that
 *  width — its height (`fit.ts`). */
async function fitCell(kernel: Kernel, view: string, cell: string, max: { width: number; height: number }): Promise<Json> {
  const first = sizeOf(cell) ?? (await nextSize(cell, FIT_WAIT_MS));
  if (first === null) {
    return { error: `view: fit — ${JSON.stringify(cell)} reported no content size`, reason: "not_ready", arg: "id", hint: "only an html cell (a panel) measures what it draws, once it is drawn" };
  }
  const theme = themeOf(kernel, view, tabWidth());
  const chrome = chromeOf(cell);
  const wide = fitSize({ width: first.mw, height: first.h }, chrome, max);
  const again = nextSize(cell, FIT_WAIT_MS);
  setRect(kernel, view, theme, cell, { width: wide.width });
  const at = (await again) ?? first;
  const f = fitSize({ width: first.mw, height: at.h }, chrome, max);
  const rect = setRect(kernel, view, theme, cell, { width: f.width, height: f.height });
  // OVERFLOWED: still too big at this size — too tall for the max, or wider than
  // the clamped width even wrapped (the panel's own measure there)
  const overflowed = at.h + chrome.height > max.height || (wide.overflowed && at.ox === true);
  return { fitted: cell, theme, ...rect, content: { width: first.mw, height: at.h }, overflowed } as unknown as Json;
}

export const CANVAS_README =
  
  "The canvas: the page's root agent. It holds the one view, `view_root` — build there (`docs of=view_root`). It answers reflect only.";

export const VIEW_README =
  
  "THE VIEW — the canvas's one view (frontend). Its cells are its children: you place and change them HERE. VERBS: place {kind: html|gl|terminal|chat (or handler_module) — or only backend_id: a VIEW of that host agent, drawn as its record says — x, y, id?, width?, height?, display_name?, bytes? (the body, verbatim), trusted?, can_call?, callable_by?, backend_id? (terminal/chat: the host agent shown)} · move {id, x?, y?, width?, height?} · fit {id, max_width?, max_height?} (an html cell sized to what it draws — {width, height, overflowed}; a cell keeps its size otherwise, 480×320 when place names none: after you place or update an html cell, call fit) · update {id, bytes? (a new body reloads it), fields?: {display_name|trusted|readme|x|y|width|height|can_call|callable_by}} (on the view itself: any field) · theme {name?: auto | a theme's name} (which theme the cells sit in — stock: canvas = the free 2D canvas, mobile = one column; the canvas theme agent lists, adds and edits themes (`themes`, `add_theme`); auto = the first theme whose width trigger matches the tab (stock mobile: under 768px wide); x/y/width/height are per theme, kept in the view's layout_<theme> child, never on a cell — a cell new to a theme is placed by it) · terminal_renderer {name?: auto | webgl | dom} (the terminals' renderer — auto: DOM in WebKitGTK, else the shared WebGL; no name: what it is and why) · wire {sender, receiver, verbs} (a grant, written on each side that is a cell; not from/to) · remove {id} (DELETES the cell from the view: its agent is untouched) · hide {id} (takes the cell OFF the view, its agent keeps running and its rect is kept; hiding a hidden cell changes nothing) · show {id, x?, y?} (puts a hidden cell back where it was — or at x, y in the theme in use — and it re-attaches live; a shown cell keeps its place). open {path | agent, x?, y?} (what a drop on the canvas does: a FILE opens as a preview pinned to it, a FOLDER as a file tree rooted at it, an agent — or an agent's folder — as its cell: a cell already on the view is moved to x, y when given (never a second one), and without x, y keeps its place; a hidden one is shown (where it was, or at x, y), else the agent is placed at x, y or 0, 0; refused: bad_arg, outside_root, not_found, or the file bridge's refusal). hide, show and remove stay distinct: hide and show never touch the agent. A PANEL'S JS (an html cell's body) has `fantastic`: `await fantastic.send(id, payload)` → the reply · `fantastic.watch(id, ev => …)` → events (returns off) · `fantastic.emit(id, payload)` · `fantastic.onMessage(p => …)`. Its calls are judged by the CELL's can_call — grant what it calls (`{<id>: [verbs…, watch]}`); a reply's `grants_missing` names any it lacks. On the host they arrive as `user`: a host agent you create for a panel needs `callable_by: {user: [<the verbs the panel calls>, watch]}` in its create_agent (a new agent admits no one); never drop `user` from an existing agent's callable_by. Example — a button that runs a command on a host exec agent `runner` and shows its output live: place {kind: html, x: 700, y: 60, can_call: {runner: [start, watch]}, bytes: <button onclick=\"fantastic.send('runner',{type:'start',command:'ls'}).then(r=>job=r.job_id)\">Run</button><pre id=o></pre><script>let job;fantastic.watch('runner',e=>{if(e.job_id===job&&e.type==='progress')o.textContent+=e.line+'\\n'})</script>} A LIVE HOST CELL — the agent pushes, its panel redraws (no polling): a bundle agent (start from `spawn {bundle: python_agent}` on the canvas's host node) whose record has `view: {kind: html, bytes_file: panel.html, calls: [get_html, watch]}` and `callable_by: {user: [get_html, watch]}`; its `agent.py` sends `fa.emit({'type': 'update', 'html': …})` on each change; its `panel.html` does `fantastic.send(id,{type:'get_html'})` once, then `fantastic.watch(id, e => { if (e.type==='update') root.innerHTML = e.html })`; show it with place {backend_id: id}. innerHTML runs no <script>: keep the panel's script in panel.html and send data (or script-free HTML) in the events. A live URL or a file: an html cell whose body is <iframe src=URL> (trusted: true for GET /static_files/file/<path>). The background is the child `background` (set_bg / default_bg / clear_bg / get_bg). ";

/** What the page does when its host stops (`main.ts` sets it): its last
 *  records flushed, nothing written after. */
let hostStopping: () => Promise<void> = async () => {};
export function onHostStopping(fn: () => Promise<void>): void {
  hostStopping = fn;
}

/** The root: `reflect` is the kernel's; besides it, only the host's
 *  `host_stopping` (its shutdown, before the drain). */
export const canvasAgent: Handler = (_id, payload): Json | Promise<Json> => {
  switch (str(payload, "type")) {
    case "boot":
    case "shutdown":
      return null;
    case "host_stopping":
      return hostStopping().then(() => ({ stopped: true }));
    default:
      return { error: `canvas: unknown verb '${str(payload, "type")}' — the canvas holds the root view (${ROOT_VIEW}); talk to a view`, reason: "unhandled", verb: str(payload, "type") };
  }
};

export const viewAgent: Handler = (id, payload, kernel): Json | Promise<Json> => {
  switch (str(payload, "type")) {
    case "place": {
      // WHAT to make: a `kind` (html|gl|terminal|chat), or the cell's own
      // `handler_module` (any page bundle; the `.ts` suffix optional) — or,
      // with only `backend_id`, a VIEW of that host agent, drawn as its own
      // record says (`view`, else its kind's default, else the stub)
      const given = str(payload, "handler_module");
      if (!given && !str(payload, "kind") && str(payload, "backend_id")) {
        if (!num(payload["x"]) || !num(payload["y"])) return { error: "view: place needs x and y (numbers, view coordinates)", reason: "bad_arg", arg: "x" };
        const agent = str(payload, "backend_id");
        // an agent made a moment ago (spawn, then place) is read from the host
        // first — the page's copy of the host agents syncs only on its own events
        return hostRecordFresh(agent).then((rec): Json => {
          if (rec === undefined) return { error: `view: place — no host agent ${JSON.stringify(agent)}`, reason: "not_found", arg: "backend_id", hint: "reflect the host kernel for the agents there" };
          const { handler, meta } = hostViewCell(agent, rec);
          const aid = str(payload, "id") || `view_${Math.random().toString(16).slice(2, 10)}`;
          if (kernel.get(aid) !== undefined) return { error: `view: place — ${aid} already exists`, reason: "exists", arg: "id" };
          if (payload["display_name"] !== undefined) meta["display_name"] = payload["display_name"];
          setRect(kernel, id, themeOf(kernel, id, tabWidth()), aid, { width: 480, height: 320, ...geometryOf(payload) });
          kernel.register(new Agent({ id: aid, parentId: id, handlerModule: handler, meta: meta as Payload }));
          return { placed: aid, shows: agent, view: viewOf(rec).kind } as Json;
        });
      }
      const named = given ? (given.endsWith(".ts") ? given : `${given}.ts`) : undefined;
      const kind = named !== undefined ? (Object.entries(PLACEABLE).find(([, m]) => m === named)?.[0] ?? named.slice(0, -3)) : str(payload, "kind");
      const hm = named ?? PLACEABLE[kind];
      if (hm === undefined) return { error: `view: place — kind ${JSON.stringify(kind)} is one of ${Object.keys(PLACEABLE).join(", ")}, or name the cell's handler_module`, reason: "bad_arg", arg: "kind" };
      if (!num(payload["x"]) || !num(payload["y"])) return { error: "view: place needs x and y (numbers, view coordinates)", reason: "bad_arg", arg: "x" };
      const backend = str(payload, "backend_id");
      if ((kind === "terminal" || kind === "chat") && !backend) {
        return { error: `view: place ${kind} needs backend_id — the host agent it shows`, reason: "bad_arg", arg: "backend_id" };
      }
      const want = str(payload, "id");
      const aid = want || `${kind}_${Math.random().toString(16).slice(2, 10)}`;
      if (kernel.get(aid) !== undefined) return { error: `view: place — ${aid} already exists`, reason: "exists", arg: "id", hint: `to change it: update {id: ${aid}, bytes?, fields?}; to replace it: remove {id: ${aid}} first` };
      const meta: Record<string, unknown> = {};
      for (const k of SETTABLE) if (payload[k] !== undefined) meta[k] = payload[k];
      // the cell's BODY is `bytes` (verbatim): html for html, the source for gl
      const body = bodyOf(kind, payload);
      if (typeof body === "object") return body as unknown as Json;
      if (body !== undefined) meta[kind === "gl" ? "gl_source" : "html"] = body;
      // its rect in the theme in use (a size from birth), written before the cell
      setRect(kernel, id, themeOf(kernel, id, tabWidth()), aid, { width: 480, height: 320, ...geometryOf(payload) });
      if (backend) Object.assign(meta, { backend_id: backend, owns_backend: false });
      // the view's OWN declared grant onto the agent it shows
      if (kind === "terminal") meta["can_call"] = { ...terminalView.grants(backend) };
      if (kind === "chat") meta["can_call"] = { ...aiView.grants(backend) };
      // the file tree and the previewer reach the project's file bridge (sidebar_view.ts is the cell's name before the file tree (#106): saved layouts still hold it, and it opens as the tree)
      if (["files_view.ts", "preview_view.ts", "sidebar_view.ts"].includes(hm) && meta["can_call"] === undefined) meta["can_call"] = fileGrant();
      kernel.register(new Agent({ id: aid, parentId: id, handlerModule: hm, meta: meta as Payload }));
      // grants the body needs and the cell lacks are SAID (the panel's refused
      // call never reaches the AI)
      const grants = typeof meta["html"] === "string" ? grantHint(aid, meta["html"] as string, kernel.get(aid)?.meta["can_call"]) : {};
      // fields `place` does not take are SAID, never dropped silently
      const known = new Set<string>(["type", "kind", "handler_module", "id", "bytes", "backend_id", ...SETTABLE, ...GEOMETRY]);
      const ignored = Object.keys(payload).filter((k) => !known.has(k));
      if (ignored.length) {
        return { placed: aid, kind, ignored, ...grants, hint: "place does not take those fields — a cell's content is its body (bytes); to show a live URL or a file, make the body an <iframe src=…> (trusted: true for the host's own files)" + (grants["hint"] ? `. Also: ${grants["hint"]}` : "") } as Json;
      }
      return { placed: aid, kind, ...grants } as Json;
    }
    case "move": {
      const target = str(payload, "id");
      if (kernel.get(target)?.parentId !== id) return { error: `view: move — ${JSON.stringify(target)} is not a cell of this view`, reason: "not_found", arg: "id" };
      const m = geometryOf(payload);
      if (Object.keys(m).length === 0) return { error: "view: move needs x, y, width or height", reason: "bad_arg", arg: "x" };
      const theme = themeOf(kernel, id, tabWidth());
      return { moved: target, theme, ...setRect(kernel, id, theme, target, m) } as unknown as Json;
    }
    case "fit": {
      const target = str(payload, "id");
      if (kernel.get(target)?.parentId !== id) return { error: `view: fit — ${JSON.stringify(target)} is not a cell of this view`, reason: "not_found", arg: "id" };
      const max = {
        width: num(payload["max_width"]) ? (payload["max_width"] as number) : FIT_MAX.width,
        height: num(payload["max_height"]) ? (payload["max_height"] as number) : FIT_MAX.height,
      };
      return fitCell(kernel, id, target, max);
    }
    case "update": {
      const target = str(payload, "id");
      if (target === id) return updateSelf(id, payload, kernel);
      if (kernel.get(target)?.parentId !== id) return { error: `view: update — ${JSON.stringify(target)} is not a cell of this view`, reason: "not_found", arg: "id" };
      const fields = payload["fields"] ?? {};
      if (typeof fields !== "object" || Array.isArray(fields)) return { error: "view: update — fields must be {…}", reason: "bad_arg", arg: "fields" };
      if (Object.keys(fields).length === 0 && payload["bytes"] === undefined) return { error: "view: update needs bytes (a new body) or fields", reason: "bad_arg", arg: "fields" };
      const settable: readonly string[] = [...SETTABLE, ...GEOMETRY];
      const bad = Object.keys(fields).filter((k) => !settable.includes(k));
      if (bad.length) return { error: `view: update — not settable: ${bad.join(", ")} (settable: ${settable.join(", ")})`, reason: "bad_arg", arg: "fields" };
      const f = { ...(fields as Record<string, unknown>) };
      // geometry goes to the layout of the theme in use, never onto the cell
      const g = geometryOf(f);
      for (const k of GEOMETRY) delete f[k];
      if (Object.keys(g).length > 0) setRect(kernel, id, themeOf(kernel, id, tabWidth()), target, g);
      const kindOf = kernel.get(target)?.record().handler_module === "gl_agent.ts" ? "gl" : "html";
      const body = bodyOf(kindOf, payload);
      if (typeof body === "object") return body as unknown as Json;
      if (body !== undefined) f[kindOf === "gl" ? "gl_source" : "html"] = body;
      for (const k of ["html", "gl_source"]) {
        if (f[k] !== undefined && typeof f[k] !== "string") return { error: `view: update — ${k} must be a string`, reason: "bad_arg", arg: "bytes", hint: "send the markup as <bytes>…</bytes> — verbatim, nothing escaped" };
      }
      if (Object.keys(f).length > 0) kernel.updateMeta(target, f as Payload);
      // the cell's NEW BODY shows at once (owner 2026-10-13): its own inbox is told
      // directly — the view just rewrote the cell's record, its own child's; an
      // `emit` would be gated as the view's call ONTO the cell and dropped unless
      // the cell happened to grant `emit` (a panel whose can_call/callable_by an
      // AI set kept its old body until the page was reloaded)
      if ("html" in f) kernel.deliverInbox(target, { type: "reload_html" });
      const cell = kernel.get(target);
      const html = cell?.meta["html"];
      const grants = typeof html === "string" ? grantHint(target, html, cell?.meta["can_call"]) : {};
      return { updated: target, fields: Object.keys(fields), ...grants } as Json;
    }
    case "wire": {
      // a GRANT: `sender` may call `receiver` with `verbs`, merged into each
      // side that is a cell of this view (the view is its parent; another
      // agent's record is not the view's to write). (Not from/to: `to` is
      // ai_core's routing tag.)
      const [from, to] = [str(payload, "sender"), str(payload, "receiver")];
      const verbs = payload["verbs"];
      for (const [k, v] of [["sender", from], ["receiver", to]] as const) {
        if (!v) return { error: `view: wire needs ${k} (an agent id)`, reason: "bad_arg", arg: k };
      }
      const mine = (aid: string): boolean => kernel.get(aid)?.parentId === id;
      if (!mine(from) && !mine(to)) return { error: `view: wire — neither ${JSON.stringify(from)} nor ${JSON.stringify(to)} is a cell of this view`, reason: "not_found", arg: "sender" };
      if (!Array.isArray(verbs) || verbs.length === 0 || !verbs.every((x) => typeof x === "string")) {
        return { error: "view: wire needs verbs: [\"emit\", …]", reason: "bad_arg", arg: "verbs" };
      }
      const merge = (aid: string, field: string, key: string): void => {
        const cur = ((kernel.get(aid)?.meta[field] ?? {}) as Record<string, unknown>);
        const had = Array.isArray(cur[key]) ? (cur[key] as string[]) : [];
        kernel.updateMeta(aid, { [field]: { ...cur, [key]: [...new Set([...had, ...(verbs as string[])])] } } as unknown as Payload);
      };
      if (mine(from)) merge(from, "can_call", to);
      if (mine(to)) merge(to, "callable_by", from);
      return { wired: { sender: from, receiver: to, verbs } };
    }
    case "remove": {
      const target = str(payload, "id");
      if (kernel.get(target)?.parentId !== id) return { error: `view: remove — ${JSON.stringify(target)} is not a cell of this view`, reason: "not_found", arg: "id" };
      const out = kernel.remove(target) as unknown as Json;
      // a hidden cell removed is no longer hidden (its id is gone with it)
      if (hiddenOf(kernel, id).has(target)) setHidden(kernel, id, target, false);
      return out;
    }
    case "hide": {
      // OFF the view, the agent running on (#133): its record and its rect stay;
      // the page draws the view without it (its frame goes, its stream with it)
      const target = str(payload, "id");
      const cell = kernel.get(target);
      if (cell?.parentId !== id) return { error: `view: hide — ${JSON.stringify(target)} is not a cell of this view`, reason: "not_found", arg: "id" };
      if (!isCellHandler(String(cell.record().handler_module ?? ""))) return { error: `view: hide — ${JSON.stringify(target)} is not a cell (the view's own background or layout)`, reason: "bad_arg", arg: "id" };
      setHidden(kernel, id, target, true);
      return { hidden: target, hidden_cells: [...hiddenOf(kernel, id)] };
    }
    case "show": {
      // BACK on the view, where it was (or at x, y in the theme in use): the page
      // mounts it again, live — the agent's stream re-attaches
      const target = str(payload, "id");
      const cell = kernel.get(target);
      if (cell?.parentId !== id) return { error: `view: show — ${JSON.stringify(target)} is not a cell of this view`, reason: "not_found", arg: "id" };
      if (!isCellHandler(String(cell.record().handler_module ?? ""))) return { error: `view: show — ${JSON.stringify(target)} is not a cell (the view's own background or layout)`, reason: "bad_arg", arg: "id" };
      // a shown cell keeps its place: x, y place a HIDDEN cell only (`move` moves one)
      const wasHidden = hiddenOf(kernel, id).has(target);
      const at = wasHidden ? geometryOf({ x: payload["x"], y: payload["y"] }) : {};
      const theme = themeOf(kernel, id, tabWidth());
      const rect = Object.keys(at).length > 0 ? setRect(kernel, id, theme, target, at) : undefined;
      setHidden(kernel, id, target, false);
      return { shown: target, theme, ...(rect ?? {}), hidden_cells: [...hiddenOf(kernel, id)] } as unknown as Json;
    }
    case "open":
      return openView(id, payload, kernel);
    // THE TREE SIDEBAR (#148): its verbs, through the view — `tree {verb, path | agent}`
    case "tree":
      return filesAgent(SIDEBAR_ID, { ...payload, type: str(payload, "verb") }, kernel);
    case "theme": {
      // THE THEME: which layout the cells sit in (canvas | mobile), or `auto` —
      // per tab, mobile under 768px wide. No name: what it is.
      const name = payload["name"];
      if (name !== undefined) {
        const known = themesOf(kernel, id).map((d) => d.name);
        if (name !== "auto" && !(typeof name === "string" && known.includes(name))) return { error: `view: theme — name is auto or one of ${known.join(", ")}`, reason: "bad_arg", arg: "name", hint: "the canvas theme agent lists and adds themes (`themes`, `add_theme`)" };
        kernel.updateMeta(id, { theme: name });
      }
      const choice = kernel.get(id)?.meta["theme"] ?? "auto";
      return { theme: choice as Json, in_use: themeOf(kernel, id, tabWidth()), themes: ["auto", ...themesOf(kernel, id).map((d) => d.name)] };
    }
    case "terminal_renderer": {
      // THE TERMINALS' RENDERER: auto (per tab — DOM in WebKitGTK, else the
      // shared WebGL), webgl or dom. Every terminal of the view switches live.
      // No name: what it is, what is in use here and why.
      const name = payload["name"];
      if (name !== undefined) {
        if (!(RENDERER_CHOICES as readonly unknown[]).includes(name)) return { error: "view: terminal_renderer — name is auto, webgl or dom", reason: "bad_arg", arg: "name" };
        kernel.updateMeta(id, { terminal_renderer: name } as unknown as Payload);
      }
      const choice = kernel.get(id)?.meta["terminal_renderer"] ?? "auto";
      let r: ReturnType<typeof resolveRenderer>;
      try {
        r = resolveRenderer(choice, { shell: readShell(), ua: navigator.userAgent });
      } catch (err) {
        // a hand-edited record: refused loud, naming the stored value
        return { error: `view: ${(err as Error).message} (the view's stored value)`, reason: "bad_arg", arg: "terminal_renderer", stored: choice as Json, hint: "set it: terminal_renderer {name: auto | webgl | dom}" };
      }
      return { terminal_renderer: choice as Json, in_use: r.in_use, why: r.why, choices: [...RENDERER_CHOICES] };
    }
    case "boot":
    case "shutdown":
      return null;
    default:
      return { error: `view: unknown verb '${str(payload, "type")}'`, reason: "unhandled", verb: str(payload, "type") };
  }
};

// ─── `open`: WHAT A DROP ON THE CANVAS DOES (#134, #135) ─────────────────────
// A file opens as a preview pinned to it, a folder as a file tree rooted at it,
// an agent (or an agent's folder) as its cell. The canvas's drop and an AI's `open`
// both come here, so they cannot disagree. Resolution is `files/drag.ts`.

const freshId = (prefix: string): string => `${prefix}_${Math.random().toString(16).slice(2, 10)}`;

/** The file bridge verbs the VIEW asks: `list`, to tell a file from a folder by its parent's listing, and `open_view`, to make or reuse the file or folder agent a drop names (#150). A handler's host call is its own grant, and an agent never raises its own: the canvas writes this onto the root view at mount (`grantViewLists`). */
export const VIEW_LISTS = { files: ["list", "open_view"] } as const;

/** Grant every view (the root and each nested one) the listings `open` needs: the canvas is the view's parent, so its grants come from above. Idempotent: a view already granted is left alone. */
export function grantViewLists(kernel: Kernel): void {
  const walk = (a: Agent | undefined): void => {
    if (a === undefined) return;
    if (a.record()["handler_module"] === VIEW) {
      const had = (a.meta["can_call"] ?? {}) as Record<string, unknown>;
      const lists = Array.isArray(had["files"]) ? (had["files"] as unknown[]) : [];
      if (!VIEW_LISTS.files.every((v) => lists.includes(v))) kernel.updateMeta(a.id, { can_call: { ...had, files: [...lists, ...VIEW_LISTS.files] } } as unknown as Payload);
    }
    for (const c of a.children.values()) walk(c);
  };
  walk(kernel.rootId === null ? undefined : kernel.get(kernel.rootId));
}

/** The disk and the host, as `open` looks at them: a path's kind through the file bridge (its listing), the agent folders, the host's agents. */
function viewLook(kernel: Kernel): Look {
  const bridge = kernelBridge(kernel);
  return {
    kindOf: async (p) => {
      const r = (await bridge.call({ type: "list", path: wire(parentOf(p)) })) as Record<string, unknown>;
      // a folder that is not there is a path not in the project; any other refusal is said loudly
      if (isRefusal(r)) {
        if (r["reason"] === "not_found") return undefined;
        throw new Error(said(r));
      }
      const files = Array.isArray(r["files"]) ? (r["files"] as Array<Record<string, unknown>>) : [];
      const hit = files.find((f) => norm(String(f["path"])) === p);
      return hit === undefined ? undefined : hit["type"] === "dir" ? "dir" : "file";
    },
    agentAt: (p) => agentIndex.agentAt(p)?.id,
    hasAgent: async (id) => (await hostRecordFresh(id)) !== undefined,
  };
}

/** The cells of this view that show host agent `agent`. */
function cellsOfAgent(kernel: Kernel, view: string, agent: string): string[] {
  const v = kernel.get(view);
  if (v === undefined) return [];
  return [...v.children.values()].filter((c) => c.record()["backend_id"] === agent).map((c) => c.id);
}

/** `open {path | agent, x?, y?}`: resolve what it names, then open it where it lands. */
async function openView(view: string, payload: Payload, kernel: Kernel): Promise<Json> {
  const agent = str(payload, "agent");
  const path = payload["path"];
  if ((agent !== "") === (typeof path === "string")) {
    return { error: "view: open needs path (a project path) or agent (an agent id): one of them", reason: "bad_arg", arg: "path" };
  }
  const x = payload["x"];
  const y = payload["y"];
  if (x !== undefined || y !== undefined) {
    if (!num(x) || !num(y)) return { error: "view: open — x and y are numbers (the cell's corner in the view), given together", reason: "bad_arg", arg: "x" };
  }
  // x, y: where the cell goes. Absent, a cell already on the view keeps its place (the same as `show`).
  const at = num(x) && num(y) ? { x, y } : undefined;
  const target: Target = agent !== "" ? { kind: "agent", id: agent } : { kind: "any", path: path as string };
  let r: Opened | Refusal;
  try {
    r = await resolveTarget(target, viewLook(kernel));
  } catch (err) {
    // the file bridge refused the listing: the path is not one it can look at
    return { error: `view: open — ${(err as Error).message}`, reason: "refused", arg: "path", hint: "the file bridge would not list it" };
  }
  if (isRefusalOf(r)) return r as unknown as Json;
  if (r.kind === "agent") return openAgent(view, r.agent, at, kernel);
  // a new file or folder cell lands at x, y, or the corner
  if (r.kind === "file") return openFile(view, r.path, at ?? ORIGIN, kernel);
  return openDir(view, r.path, at ?? ORIGIN, kernel);
}

/** The corner a new cell lands at when `open` names no x, y. */
const ORIGIN = { x: 0, y: 0 };

/** An agent's cell: one already on the view is MOVED to x, y when they are given (a hidden one shown, there or where it was), else placed there. Never a second cell. */
async function openAgent(view: string, agent: string, at: { x: number; y: number } | undefined, kernel: Kernel): Promise<Json> {
  const mine = cellsOfAgent(kernel, view, agent);
  if (mine.length > 0) {
    const cell = mine[0]!;
    const wasHidden = hiddenOf(kernel, view).has(cell);
    // no x, y: the rect stays as it is (`show` keeps a cell's place too)
    if (at !== undefined) setRect(kernel, view, themeOf(kernel, view, tabWidth()), cell, { x: at.x, y: at.y });
    if (wasHidden) setHidden(kernel, view, cell, false);
    return { opened: agent, kind: "agent", cell, moved: at !== undefined, shown: wasHidden } as Json;
  }
  const spot = at ?? ORIGIN;
  const got = (await viewAgent(view, { type: "place", backend_id: agent, x: spot.x, y: spot.y } as unknown as Payload, kernel)) as Record<string, unknown>;
  if (typeof got["placed"] !== "string") return got as Json;
  return { opened: agent, kind: "agent", cell: got["placed"], placed: true } as Json;
}

/** A file: a preview cell pinned to it, as the view of the file's agent (made or reused on the bridge: one agent per path). It shows the file live, and does not follow the tree. */
async function openFile(view: string, path: string, at: { x: number; y: number }, kernel: Kernel): Promise<Json> {
  const agent = await openViewAgent(kernel, "file", path);
  if (isRefusal(agent)) return agent as unknown as Json;
  const aid = freshId("preview");
  kernel.register(new Agent({
    id: aid,
    parentId: view,
    handlerModule: "preview_view.ts",
    meta: { display_name: nameOf(path), pinned: path, follow: false, can_call: fileGrant(), view_agent: agent.view } as unknown as Payload,
  }));
  // its rect AFTER the cell is there: the layout's write refreshes the page, and a refresh prunes
  // the rect of a cell it does not hold yet (#148: the drop's point was lost to a free spot)
  setRect(kernel, view, themeOf(kernel, view, tabWidth()), aid, { x: at.x, y: at.y, width: 520, height: 360 });
  return { opened: path, kind: "file", cell: aid, agent: agent.view, created: agent.created } as Json;
}

/** A folder: a file tree cell rooted at it (nothing above it is shown), as the view of the folder's agent (made or reused on the bridge). */
async function openDir(view: string, path: string, at: { x: number; y: number }, kernel: Kernel): Promise<Json> {
  const agent = await openViewAgent(kernel, "folder", path);
  if (isRefusal(agent)) return agent as unknown as Json;
  const aid = freshId("files");
  kernel.register(new Agent({
    id: aid,
    parentId: view,
    handlerModule: "files_view.ts",
    meta: { display_name: path === "" ? "project" : nameOf(path), root: path, can_call: fileGrant(), view_agent: agent.view } as unknown as Payload,
  }));
  // its rect after the cell is there (as `openFile`: a refresh prunes the rect of a cell not yet held)
  setRect(kernel, view, themeOf(kernel, view, tabWidth()), aid, { x: at.x, y: at.y, width: 320, height: 560 });
  return { opened: path, kind: "dir", cell: aid, agent: agent.view, created: agent.created } as Json;
}

/** `update` on the view itself: any fields. */
function updateSelf(id: string, payload: Payload, kernel: Parameters<Handler>[2]): Json {
  const fields = payload["fields"];
  if (typeof fields !== "object" || fields === null || Array.isArray(fields) || Object.keys(fields).length === 0) {
    return { error: "view: update on the view itself needs fields: {…}", reason: "bad_arg", arg: "fields" };
  }
  const f = fields as Record<string, unknown>;
  kernel.updateMeta(id, f as unknown as Payload);
  return { updated: id, fields: Object.keys(f) };
}

/** Register the canvas root's bundle and the view bundle. */
export function registerCanvas(kernel: {
  registerBundle(hm: string, h: Handler): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(CANVAS, canvasAgent);
  kernel.setBundleReadme(CANVAS, CANVAS_README);
  kernel.registerBundle(VIEW, viewAgent);
  kernel.setBundleReadme(VIEW, VIEW_README);
}
