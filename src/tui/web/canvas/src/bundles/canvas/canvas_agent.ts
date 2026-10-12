import { Agent, str } from "../../lib.ts";
import { mcpView } from "../mcp_view/mcp_view.ts";
import type { Handler, Json, Payload } from "../../lib.ts";
import { terminalView } from "../terminal_view/terminal_view.ts";
import { bodyText } from "../body_text.ts";
import { htmlRefusal } from "../html_check.ts";
import { aiView } from "../ai_view/ai_view.ts";
import { grantHint } from "../html_agent/grant_hint.ts";
import { reflectView } from "../reflect_view/reflect_view.ts";
import { cellGrant, hostRecordFresh, viewOf } from "./host_views.ts";
import type { Rec } from "./host_views.ts";
import { setRect, tabWidth, themeOf, themesOf } from "./layout.ts";
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
// `wire`, `remove`, `theme`. The page kernel has no tree-CRUD verbs by design;
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
export const VIEW_VERBS = ["place", "move", "update", "wire", "remove", "theme", "reflect"] as const;

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


export const CANVAS_README =
  
  "The canvas: the page's root agent. It holds the one view, `view_root` — build there (`docs of=view_root`). It answers reflect only.";

export const VIEW_README =
  
  "THE VIEW — the canvas's one view (frontend). Its cells are its children: you place and change them HERE. VERBS: place {kind: html|gl|terminal|chat (or handler_module) — or only backend_id: a VIEW of that host agent, drawn as its record says — x, y, id?, width?, height?, display_name?, bytes? (the body, verbatim), trusted?, can_call?, callable_by?, backend_id? (terminal/chat: the host agent shown)} · move {id, x?, y?, width?, height?} · update {id, bytes? (a new body reloads it), fields?: {display_name|trusted|readme|x|y|width|height|can_call|callable_by}} (on the view itself: any field) · theme {name?: auto | a theme's name} (which theme the cells sit in — stock: canvas = the free 2D canvas, mobile = one column; the canvas theme agent lists, adds and edits themes (`themes`, `add_theme`); auto = the first theme whose width trigger matches the tab (stock mobile: under 768px wide); x/y/width/height are per theme, kept in the view's layout_<theme> child, never on a cell — a cell new to a theme is placed by it) · wire {sender, receiver, verbs} (a grant, written on each side that is a cell; not from/to) · remove {id}. A PANEL'S JS (an html cell's body) has `fantastic`: `await fantastic.send(id, payload)` → the reply · `fantastic.watch(id, ev => …)` → events (returns off) · `fantastic.emit(id, payload)` · `fantastic.onMessage(p => …)`. Its calls are judged by the CELL's can_call — grant what it calls (`{<id>: [verbs…, watch]}`); a reply's `grants_missing` names any it lacks. On the host they arrive as `user`: a host agent you create for a panel needs `callable_by: {user: [<the verbs the panel calls>, watch]}` in its create_agent (a new agent admits no one); never drop `user` from an existing agent's callable_by. Example — a button that runs a command on a host exec agent `runner` and shows its output live: place {kind: html, x: 700, y: 60, can_call: {runner: [start, watch]}, bytes: <button onclick=\"fantastic.send('runner',{type:'start',command:'ls'}).then(r=>job=r.job_id)\">Run</button><pre id=o></pre><script>let job;fantastic.watch('runner',e=>{if(e.job_id===job&&e.type==='progress')o.textContent+=e.line+'\\n'})</script>} A LIVE HOST CELL — the agent pushes, its panel redraws (no polling): a bundle agent (start from `spawn {bundle: python_agent}` on the canvas's host node) whose record has `view: {kind: html, bytes_file: panel.html, calls: [get_html, watch]}` and `callable_by: {user: [get_html, watch]}`; its `agent.py` sends `fa.emit({'type': 'update', 'html': …})` on each change; its `panel.html` does `fantastic.send(id,{type:'get_html'})` once, then `fantastic.watch(id, e => { if (e.type==='update') root.innerHTML = e.html })`; show it with place {backend_id: id}. innerHTML runs no <script>: keep the panel's script in panel.html and send data (or script-free HTML) in the events. A live URL or a file: an html cell whose body is <iframe src=URL> (trusted: true for GET /static_files/file/<path>). The background is the child `background` (set_bg / default_bg / clear_bg / get_bg). ";

/** The root: `reflect` is the kernel's; it answers nothing else. */
export const canvasAgent: Handler = (_id, payload): Json => {
  switch (str(payload, "type")) {
    case "boot":
    case "shutdown":
      return null;
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
      return out;
    }
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
    case "boot":
    case "shutdown":
      return null;
    default:
      return { error: `view: unknown verb '${str(payload, "type")}'`, reason: "unhandled", verb: str(payload, "type") };
  }
};

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
