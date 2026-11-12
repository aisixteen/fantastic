import { Agent, str } from "../../lib.ts";
import { mcpView } from "../mcp_view/mcp_view.ts";
import type { Handler, Json, Kernel, Payload } from "../../lib.ts";
import { terminalView } from "../terminal_view/terminal_view.ts";
import { RENDERER_CHOICES, resolveRenderer } from "../terminal_view/renderer.ts";
import { readShell } from "../shell.ts";
import { bodyText } from "../body_text.ts";
import { htmlRefusal } from "../html_check.ts";
import { docsOf, entry } from "../declared.ts";
import type { Declared } from "../declared.ts";
import { fileGrant, isRefusal, kernelBridge, openViewAgent, said } from "../files/file_bridge.ts";
import { agentIndex } from "../files/files_deps.ts";
import { isRefusalOf, resolveTarget } from "../files/drag.ts";
import type { Look, Opened, Refusal, Target } from "../files/drag.ts";
import { nameOf, norm, parentOf, wire } from "../files/paths.ts";
import { grantHint } from "../html_agent/grant_hint.ts";
import { reflectView } from "../reflect_view/reflect_view.ts";
import { cellGrant, hostRecordFresh, viewOf, viewRecordFresh } from "./host_views.ts";
import type { Rec } from "./host_views.ts";
import { hiddenOf, layoutFor, setHidden, setRect, tabWidth } from "./layout.ts";
import { isCellHandler } from "./cell_menu.ts";
import { chromeOf, FIT_MAX, fitSize, nextSize, sizeOf } from "./fit.ts";
import type { Rect } from "./layout.ts";

/** The cell meta that draws host agent `agent` by its own view (`host_views.ts`). */
export function hostViewCell(agent: string, rec: Rec): { handler: string; meta: Record<string, unknown> } {
  const v = viewOf(rec);
  const meta: Record<string, unknown> = { backend_id: agent, owns_backend: false };
  // the renderer's OWN grant onto its agent (terminal and the stub declare
  // theirs); an html view's is its record's `calls`
  meta["can_call"] =
    v.kind === "terminal" ? { ...terminalView.grants(agent) }
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
// `wire`, `remove`, `terminal_renderer`. The page kernel has no tree-CRUD verbs by design;
// the view, which owns its members, answers these. Its background is its own
// singleton child agent (`background_agent.ts`); where its cells sit is its
// layout children (`layout.ts`).

/** The page's root agent. */
export const CANVAS = "canvas.ts";
/** A view: a container of cells. */
export const VIEW = "view.ts";
/** The root view's id: every page has exactly one, under the root. */
export const ROOT_VIEW = "view_root";

/** The kinds `place` makes: page-side content cells, and a view of an existing
 *  host agent (a terminal onto an exec agent) — weak peers. */
export const PLACEABLE: Record<string, string> = {
  html: "html_agent.ts",
  gl: "gl_agent.ts",
  terminal: "terminal_view.ts",
};

/** Fields `place` / `update` may set — a cell's grants included (the view is
 *  its parent); never identity, placement in the tree or a view's binding. */
export const SETTABLE = ["display_name", "html", "gl_source", "trusted", "readme", "can_call", "callable_by"] as const;

/** A cell's GEOMETRY: not on its record — in the view's layout
 *  (`layout.ts`). `place`, `move` and `update` take these and write them there. */
export const GEOMETRY = ["x", "y", "width", "height"] as const;

/** Every view verb, for the grants that admit a caller. */
export const VIEW_VERBS = ["place", "move", "fit", "update", "wire", "remove", "hide", "show", "open", "terminal_renderer", "reflect"] as const;

const num = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

/** The geometry fields of a call, as a rect patch. */
const geometryOf = (p: Record<string, unknown>): Partial<Rect> => {
  const m: Partial<Rect> = {};
  for (const k of GEOMETRY) if (num(p[k])) m[k] = p[k] as number;
  return m;
};

/** A cell's BODY from a call: `bytes` (ai_core's verbatim convention), or the
 *  kind's own field (`html` / `gl_source`), both declared as text. undefined =
 *  none given; a refusal when an html body is no html. */
function bodyOf(kind: string, payload: Payload): string | undefined | Record<string, unknown> {
  const field = kind === "gl" ? "gl_source" : "html";
  const given = payload["bytes"] ?? payload[field];
  if (given === undefined) return undefined;
  const raw = bodyText(given) as string;
  // an html body must BE html (a placeholder, a truncated script: refused)
  if (field === "html") return htmlRefusal("view", raw) ?? raw;
  return raw;
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
  const layout = layoutFor(tabWidth());
  const chrome = chromeOf(cell);
  const wide = fitSize({ width: first.mw, height: first.h }, chrome, max);
  const again = nextSize(cell, FIT_WAIT_MS);
  setRect(kernel, view, layout, cell, { width: wide.width });
  const at = (await again) ?? first;
  const f = fitSize({ width: first.mw, height: at.h }, chrome, max);
  const rect = setRect(kernel, view, layout, cell, { width: f.width, height: f.height });
  // OVERFLOWED: still too big at this size — too tall for the max, or wider than
  // the clamped width even wrapped (the panel's own measure there)
  const overflowed = at.h + chrome.height > max.height || (wide.overflowed && at.ox === true);
  return { fitted: cell, ...rect, content: { width: first.mw, height: at.h }, overflowed } as unknown as Json;
}

export const CANVAS_MAN = "The canvas: the page's root agent. It holds the one view, `view_root`; build there.";

export const VIEW_MAN =
  "THE VIEW: the canvas's one view. Its cells are its children; you place and change them here. A cell's html body runs with `fantastic` (`send`, `watch`, `emit`, `onMessage`): its calls are judged by the cell's can_call, and on the host they arrive as `user`.";

const GEOMETRY_ARGS = {
  x: { type: "number", doc: "The cell's left edge in the view." },
  y: { type: "number", doc: "The cell's top edge in the view." },
  width: { type: "number", min: 1, doc: "The cell's width." },
  height: { type: "number", min: 1, doc: "The cell's height." },
} as const;

/** The verbs the view answers, declared once: the call is checked against them before the handler
 *  runs (`declared.ts`) and `docs` is rendered from them. */
export const VIEW_DECL: Declared = {
  place: {
    doc: "A new cell of this view: a `kind` (or a `handler_module`), or only `backend_id` for a view of that host agent, drawn as its record says. Returns {placed, kind, grants_missing?, hint?}.",
    args: {
      kind: { type: "string", enum: Object.keys(PLACEABLE), doc: "What to make." },
      handler_module: { type: "string", min: 1, doc: "The cell's own bundle (the `.ts` suffix optional), instead of a kind." },
      backend_id: { type: "string", min: 1, doc: "The host agent the cell shows (a terminal's agent), or alone: a view of that agent." },
      x: { ...GEOMETRY_ARGS.x, required: true },
      y: { ...GEOMETRY_ARGS.y, required: true },
      width: GEOMETRY_ARGS.width,
      height: GEOMETRY_ARGS.height,
      id: { type: "string", min: 1, doc: "The new cell's id; absent, one is made." },
      bytes: { type: "bytes", doc: "The cell's body, verbatim: html for an html cell, the source for a gl cell." },
      html: { type: "bytes", doc: "An html cell's body, as its record field (`bytes` is the same body)." },
      gl_source: { type: "bytes", doc: "A gl cell's source, as its record field (`bytes` is the same body)." },
      display_name: { type: "string", doc: "The cell's title." },
      trusted: { type: "boolean", doc: "An html cell runs unsandboxed (a live URL or the host's own files)." },
      readme: { type: "string", doc: "What the cell is for." },
      can_call: { type: "object", doc: "What the cell may call: {<id>: [verbs…, watch]}; a reply's `grants_missing` names what its body calls and it lacks." },
      callable_by: { type: "object", doc: "Who may call the cell." },
    },
    anyOf: [["kind"], ["handler_module"], ["backend_id"]],
    when: [
      { arg: "kind", is: "terminal", then: "backend_id" },
      { arg: "handler_module", is: "terminal_view", then: "backend_id" },
      { arg: "handler_module", is: "terminal_view.ts", then: "backend_id" },
    ],
  },
  move: {
    doc: "Moves or resizes a cell. Returns {moved, …}.",
    args: { id: { type: "string", required: true, min: 1, doc: "The cell." }, ...GEOMETRY_ARGS },
    anyOf: [["x"], ["y"], ["width"], ["height"]],
  },
  fit: {
    doc: "Sizes an html cell to what it draws: the width its content wants, then its height at that width. Returns {fitted, x, y, width, height, content, overflowed}; call it after placing or updating an html cell.",
    args: {
      id: { type: "string", required: true, min: 1, doc: "The cell." },
      max_width: { type: "number", min: 1, default: FIT_MAX.width, doc: "The widest it may get." },
      max_height: { type: "number", min: 1, default: FIT_MAX.height, doc: "The tallest it may get." },
    },
  },
  update: {
    doc: "Changes a cell (or this view): a new body, and/or record fields. Returns {updated, fields, grants_missing?, hint?}.",
    args: {
      id: { type: "string", required: true, min: 1, doc: "A cell, or this view." },
      bytes: { type: "bytes", doc: "A new body, verbatim." },
      fields: { type: "object", doc: `The fields to set: ${[...SETTABLE, ...GEOMETRY].join(", ")} on a cell; any field on the view.` },
    },
    anyOf: [["bytes"], ["fields"]],
  },
  wire: {
    doc: "A grant: `sender` may call `receiver` with `verbs`, written on each side that is a cell of this view (sender's can_call, receiver's callable_by).",
    args: {
      sender: { type: "string", required: true, min: 1, doc: "The agent that calls." },
      receiver: { type: "string", required: true, min: 1, doc: "The agent called." },
      verbs: { type: "string[]", required: true, min: 1, doc: "The verbs granted." },
    },
  },
  remove: {
    doc: "Removes a cell from the view; its agent is untouched; refused on a delete_lock (the background). Returns {deleted}.",
    args: { id: { type: "string", required: true, min: 1, doc: "The cell." } },
  },
  hide: {
    doc: "Takes the cell off the view: its agent keeps running, its rect is kept, a reload keeps it hidden; idempotent. Returns {hidden, hidden_cells}.",
    args: { id: { type: "string", required: true, min: 1, doc: "The cell." } },
  },
  show: {
    doc: "Puts a hidden cell back on the view, live: where it was, or at x, y (a shown cell keeps its place; `move` moves one). Returns {shown, x?, y?, width?, height?, hidden_cells}.",
    args: { id: { type: "string", required: true, min: 1, doc: "The cell." }, x: GEOMETRY_ARGS.x, y: GEOMETRY_ARGS.y },
  },
  open: {
    doc: "What a drop on the canvas does: a FILE opens as a preview pinned to it, a FOLDER as a file tree rooted at it, an agent (or an agent's folder) as its cell: one already on the view is moved to x, y when given (never a second), a hidden one is shown. Returns {opened, kind: agent|file|dir, cell, moved?, shown?, placed?}.",
    args: {
      path: { type: "string", doc: "A project path." },
      agent: { type: "string", min: 1, doc: "An agent id." },
      x: GEOMETRY_ARGS.x,
      y: GEOMETRY_ARGS.y,
    },
    oneOf: ["path", "agent"],
    together: [["x", "y"]],
  },
  terminal_renderer: {
    doc: "The terminals' renderer: auto (DOM in WebKitGTK, else the shared WebGL), webgl or dom. No name: what it is and why.",
    args: { name: { type: "string", enum: RENDERER_CHOICES, doc: "The renderer to use." } },
  },
};

/** What the page does when its host stops (`main.ts` sets it): its last
 *  records flushed, nothing written after. */
let hostStopping: () => Promise<void> = async () => {};
export function onHostStopping(fn: () => Promise<void>): void {
  hostStopping = fn;
}

/** The verbs the root answers, besides `reflect` (the kernel's). */
export const CANVAS_DECL: Declared = {
  host_stopping: {
    doc: "The host is stopping (its shutdown, before the drain): the page flushes its last records and writes nothing after. Returns {stopped}.",
    args: {},
  },
};

const canvasHandler: Handler = (_id, payload): Json | Promise<Json> => {
  switch (str(payload, "type")) {
    case "boot":
    case "shutdown":
      return null;
    case "host_stopping":
      return hostStopping().then(() => ({ stopped: true }));
    default:
      throw new Error(`canvas: ${str(payload, "type")} is declared and not handled`);
  }
};

export const canvasAgent: Handler = entry("canvas", CANVAS_DECL, canvasHandler, ["boot", "shutdown"]);

const viewHandler: Handler = (id, payload, kernel): Json | Promise<Json> => {
  switch (str(payload, "type")) {
    case "place": {
      // WHAT to make: a `kind` (html|gl|terminal), or the cell's own
      // `handler_module` (any page bundle; the `.ts` suffix optional) — or,
      // with only `backend_id`, a VIEW of that host agent, drawn as its own
      // record says (`view`, else its kind's default, else the stub)
      const given = str(payload, "handler_module");
      if (!given && !str(payload, "kind") && str(payload, "backend_id")) {
        const agent = str(payload, "backend_id");
        // an agent made a moment ago (spawn, then place) is read from the host
        // first — the page's copy of the host agents syncs only on its own events
        return viewRecordFresh(agent).then((rec): Json => {
          if (rec === undefined) return { error: `view: place — no host agent ${JSON.stringify(agent)}`, reason: "not_found", arg: "backend_id", hint: "reflect the host kernel for the agents there" };
          const { handler, meta } = hostViewCell(agent, rec);
          const aid = str(payload, "id") || `view_${Math.random().toString(16).slice(2, 10)}`;
          if (kernel.get(aid) !== undefined) return { error: `view: place — ${aid} already exists`, reason: "exists", arg: "id" };
          if (payload["display_name"] !== undefined) meta["display_name"] = payload["display_name"];
          setRect(kernel, id, layoutFor(tabWidth()), aid, { width: 480, height: 320, ...geometryOf(payload) });
          kernel.register(new Agent({ id: aid, parentId: id, handlerModule: handler, meta: meta as Payload }));
          return { placed: aid, shows: agent, view: viewOf(rec).kind } as Json;
        });
      }
      const named = given ? (given.endsWith(".ts") ? given : `${given}.ts`) : undefined;
      const kind = named !== undefined ? (Object.entries(PLACEABLE).find(([, m]) => m === named)?.[0] ?? named.slice(0, -3)) : str(payload, "kind");
      const hm = named ?? PLACEABLE[kind]!;
      const backend = str(payload, "backend_id");
      const want = str(payload, "id");
      const aid = want || `${kind}_${Math.random().toString(16).slice(2, 10)}`;
      if (kernel.get(aid) !== undefined) return { error: `view: place — ${aid} already exists`, reason: "exists", arg: "id", hint: `to change it: update {id: ${aid}, bytes?, fields?}; to replace it: remove {id: ${aid}} first` };
      const meta: Record<string, unknown> = {};
      for (const k of SETTABLE) if (payload[k] !== undefined) meta[k] = payload[k];
      // the cell's BODY is `bytes` (verbatim): html for html, the source for gl
      const body = bodyOf(kind, payload);
      if (typeof body === "object") return body as unknown as Json;
      if (body !== undefined) meta[kind === "gl" ? "gl_source" : "html"] = body;
      // its rect in the layout in use (a size from birth), written before the cell
      setRect(kernel, id, layoutFor(tabWidth()), aid, { width: 480, height: 320, ...geometryOf(payload) });
      if (backend) Object.assign(meta, { backend_id: backend, owns_backend: false });
      // the view's OWN declared grant onto the agent it shows
      if (kind === "terminal") meta["can_call"] = { ...terminalView.grants(backend) };
      // the file tree and the previewer reach the project's file bridge
      if (["files_view.ts", "preview_view.ts"].includes(hm) && meta["can_call"] === undefined) meta["can_call"] = fileGrant();
      kernel.register(new Agent({ id: aid, parentId: id, handlerModule: hm, meta: meta as Payload }));
      // grants the body needs and the cell lacks are SAID (the panel's refused
      // call never reaches the AI)
      const grants = typeof meta["html"] === "string" ? grantHint(aid, meta["html"] as string, kernel.get(aid)?.meta["can_call"]) : {};
      return { placed: aid, kind, ...grants } as Json;
    }
    case "move": {
      const target = str(payload, "id");
      if (kernel.get(target)?.parentId !== id) return { error: `view: move — ${JSON.stringify(target)} is not a cell of this view`, reason: "not_found", arg: "id" };
      const layout = layoutFor(tabWidth());
      return { moved: target, ...setRect(kernel, id, layout, target, geometryOf(payload)) } as unknown as Json;
    }
    case "fit": {
      const target = str(payload, "id");
      if (kernel.get(target)?.parentId !== id) return { error: `view: fit — ${JSON.stringify(target)} is not a cell of this view`, reason: "not_found", arg: "id" };
      const max = { width: payload["max_width"] as number, height: payload["max_height"] as number };
      return fitCell(kernel, id, target, max);
    }
    case "update": {
      const target = str(payload, "id");
      if (target === id) return updateSelf(id, payload, kernel);
      if (kernel.get(target)?.parentId !== id) return { error: `view: update — ${JSON.stringify(target)} is not a cell of this view`, reason: "not_found", arg: "id" };
      const fields = (payload["fields"] ?? {}) as Record<string, unknown>;
      const settable: readonly string[] = [...SETTABLE, ...GEOMETRY];
      const bad = Object.keys(fields).filter((k) => !settable.includes(k));
      if (bad.length) return { error: `view: update — not settable: ${bad.join(", ")} (settable: ${settable.join(", ")})`, reason: "bad_arg", arg: "fields" };
      const f = { ...(fields as Record<string, unknown>) };
      // geometry goes to the layout in use, never onto the cell
      const g = geometryOf(f);
      for (const k of GEOMETRY) delete f[k];
      if (Object.keys(g).length > 0) setRect(kernel, id, layoutFor(tabWidth()), target, g);
      const kindOf = kernel.get(target)?.record().handler_module === "gl_agent.ts" ? "gl" : "html";
      const body = bodyOf(kindOf, payload);
      if (typeof body === "object") return body as unknown as Json;
      if (body !== undefined) f[kindOf === "gl" ? "gl_source" : "html"] = body;
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
      const verbs = payload["verbs"] as string[];
      const mine = (aid: string): boolean => kernel.get(aid)?.parentId === id;
      if (!mine(from) && !mine(to)) return { error: `view: wire — neither ${JSON.stringify(from)} nor ${JSON.stringify(to)} is a cell of this view`, reason: "not_found", arg: "sender" };
      const merge = (aid: string, field: string, key: string): void => {
        const cur = ((kernel.get(aid)?.meta[field] ?? {}) as Record<string, unknown>);
        const had = Array.isArray(cur[key]) ? (cur[key] as string[]) : [];
        kernel.updateMeta(aid, { [field]: { ...cur, [key]: [...new Set([...had, ...verbs])] } } as unknown as Payload);
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
      // BACK on the view, where it was (or at x, y in the layout in use): the page
      // mounts it again, live — the agent's stream re-attaches
      const target = str(payload, "id");
      const cell = kernel.get(target);
      if (cell?.parentId !== id) return { error: `view: show — ${JSON.stringify(target)} is not a cell of this view`, reason: "not_found", arg: "id" };
      if (!isCellHandler(String(cell.record().handler_module ?? ""))) return { error: `view: show — ${JSON.stringify(target)} is not a cell (the view's own background or layout)`, reason: "bad_arg", arg: "id" };
      // a shown cell keeps its place: x, y place a HIDDEN cell only (`move` moves one)
      const wasHidden = hiddenOf(kernel, id).has(target);
      const at = wasHidden ? geometryOf({ x: payload["x"], y: payload["y"] }) : {};
      const layout = layoutFor(tabWidth());
      const rect = Object.keys(at).length > 0 ? setRect(kernel, id, layout, target, at) : undefined;
      setHidden(kernel, id, target, false);
      return { shown: target, ...(rect ?? {}), hidden_cells: [...hiddenOf(kernel, id)] } as unknown as Json;
    }
    case "open":
      return openView(id, payload, kernel);
    case "terminal_renderer": {
      // THE TERMINALS' RENDERER: auto (per tab — DOM in WebKitGTK, else the
      // shared WebGL), webgl or dom. Every terminal of the view switches live.
      // No name: what it is, what is in use here and why.
      const name = payload["name"];
      if (name !== undefined) kernel.updateMeta(id, { terminal_renderer: name } as unknown as Payload);
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
      return null; // the page kernel's lifecycle: nothing to open or release
    default:
      throw new Error(`view: ${str(payload, "type")} is declared and not handled`);
  }
};

/** The view's entry: every call is checked against `VIEW_DECL` before `viewHandler` runs. The page
 *  kernel's lifecycle verbs (`boot`, `shutdown`) are not declared and reach the handler as they are. */
export const viewAgent: Handler = entry("view", VIEW_DECL, viewHandler, ["boot", "shutdown"]);

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
  const x = payload["x"];
  const y = payload["y"];
  // x, y: where the cell goes. Absent, a cell already on the view keeps its place (the same as `show`).
  const at = typeof x === "number" && typeof y === "number" ? { x, y } : undefined;
  const target: Target = agent !== "" ? { kind: "agent", id: agent } : { kind: "any", path: path as string };
  let r: Opened | Refusal;
  try {
    r = await resolveTarget(target, viewLook(kernel));
  } catch (err) {
    // the file bridge refused the listing: the path is not one it can look at
    return { error: `view: open — ${(err as Error).message}`, reason: "bad_arg", arg: "path", hint: "the file bridge would not list it" };
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
    if (at !== undefined) setRect(kernel, view, layoutFor(tabWidth()), cell, { x: at.x, y: at.y });
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
  setRect(kernel, view, layoutFor(tabWidth()), aid, { x: at.x, y: at.y, width: 520, height: 360 });
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
  setRect(kernel, view, layoutFor(tabWidth()), aid, { x: at.x, y: at.y, width: 320, height: 560 });
  return { opened: path, kind: "dir", cell: aid, agent: agent.view, created: agent.created } as Json;
}

/** `update` on the view itself: any fields. */
function updateSelf(id: string, payload: Payload, kernel: Parameters<Handler>[2]): Json {
  const f = (payload["fields"] ?? {}) as Record<string, unknown>;
  kernel.updateMeta(id, f as unknown as Payload);
  return { updated: id, fields: Object.keys(f) };
}

/** Register the canvas root's bundle and the view bundle. */
export function registerCanvas(kernel: {
  registerBundle(hm: string, h: Handler, verbs?: Record<string, string>): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(CANVAS, canvasAgent, docsOf(CANVAS_DECL));
  kernel.setBundleReadme(CANVAS, CANVAS_MAN);
  kernel.registerBundle(VIEW, viewAgent, docsOf(VIEW_DECL));
  kernel.setBundleReadme(VIEW, VIEW_MAN);
}
