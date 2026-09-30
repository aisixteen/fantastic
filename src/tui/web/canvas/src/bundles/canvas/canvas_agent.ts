import { Agent, str } from "../../lib.ts";
import type { Handler, Json, Payload } from "../../lib.ts";
import { terminalView } from "../terminal_view/terminal_view.ts";
import { bodyText } from "../body_text.ts";
import { aiView } from "../ai_view/ai_view.ts";

// THE CANVAS AGENT — the frontend root's own verbs: `around` (geometry, from
// the rendered frames), and the CONTROL PLANE over its own cells (owner
// 2026-09-30: an AI cell is the canvas's control plane) — `place`, `move`,
// `update`, `wire`, `remove`. The page kernel has no tree-CRUD verbs by design;
// the canvas, which owns its members, answers these. The background is its own singleton child agent
// (`background_agent.ts`).

export const CANVAS = "canvas.ts";

/** The kinds `place` makes: page-side content cells, and a view of an existing
 *  host agent (a terminal onto an exec agent, a chat onto an AI) — weak peers. */
export const PLACEABLE: Record<string, string> = {
  html: "html_agent.ts",
  gl: "gl_agent.ts",
  terminal: "terminal_view.ts",
  chat: "ai_view.ts",
};

/** Fields `place` / `update` may set — never identity, placement in the tree,
 *  grants or a view's binding. */
export const SETTABLE = ["display_name", "html", "gl_source", "trusted", "readme", "x", "y", "width", "height"] as const;

/** Every canvas verb, for the grants that admit a caller. */
export const CANVAS_VERBS = ["around", "place", "move", "update", "wire", "remove", "reflect"] as const;

const num = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

/** A cell's BODY from a call: `bytes` (ai_core's verbatim convention), or the
 *  kind's own field (`html` / `gl_source`). undefined = none given; a refusal
 *  when it is not a string (the grammar parsed markup's tags as arguments). */
function bodyOf(kind: string, payload: Payload): string | undefined | Record<string, unknown> {
  const field = kind === "gl" ? "gl_source" : "html";
  const given = payload["bytes"] ?? payload[field];
  if (given === undefined) return undefined;
  const raw = bodyText(given);
  if (raw !== null) return raw;
  return { error: `canvas: the ${field} must be a string`, reason: "bad_arg", arg: "bytes", hint: "send the markup as <bytes>…</bytes> — verbatim, nothing escaped" };
}

/** One rendered member, in canvas (world) coordinates. */
export interface Placed {
  id: string;
  kind: string;
  title: string;
  rect: { x: number; y: number; w: number; h: number };
  backend_id?: string;
}

// THE GEOMETRY is the rendered canvas's: `canvas.ts` hands its frames here
// once it is mounted. Before that there is nothing on it to be near.
let placed: (() => Placed[]) | null = null;
export function setGeometry(fn: () => Placed[]): void {
  placed = fn;
}

/** Distance from (x, y) to a rect's nearest edge — 0 inside it. */
export function distance(x: number, y: number, r: Placed["rect"]): number {
  const dx = Math.max(r.x - x, 0, x - (r.x + r.w));
  const dy = Math.max(r.y - y, 0, y - (r.y + r.h));
  return Math.round(Math.hypot(dx, dy));
}

/** `around {x, y, radius}` — the members within `radius` of the point, nearest
 *  first. GEOMETRY ONLY: reflect a member for anything more. */
export function around(members: Placed[], x: number, y: number, radius: number): Array<Placed & { distance: number }> {
  return members
    .map((m) => ({ ...m, distance: distance(x, y, m.rect) }))
    .filter((m) => m.distance <= radius)
    .sort((a, b) => a.distance - b.distance || a.id.localeCompare(b.id));
}

export const CANVAS_README =
  "Canvas compositor (frontend root). Renders the frontend's own member tree; mounts each member inline via its view bundle, or as an iframe for external content. Not a host client. Its BACKGROUND is its own singleton child agent, `background` (set_bg / clear_bg / get_bg there). `around {x:num, y:num, radius:num}` (all required; canvas coordinates, x grows right, y grows down) returns the members within `radius` of the point, nearest first: {members: [{id, kind, title, rect:{x,y,w,h}, distance, backend_id?}]} — geometry only; reflect a member for more. THE CONTROL PLANE over its cells: `place {kind: html|gl|terminal|chat, x, y, id?, width?, height?, display_name?, bytes? (the cell's body, VERBATIM: html for html, the GL source for gl), trusted?, backend_id? (terminal/chat: the host agent shown)}` → {placed}; `move {id, x?, y?, width?, height?}`; `update {id, bytes? (the new body, verbatim), fields?: {display_name|trusted|readme|x|y|width|height}}` (a new html body reloads the panel); `wire {sender, receiver, verbs}` (a grant between two cells, both sides — e.g. verbs [emit] lets the sender's panel emit to the receiver's); `remove {id}` (refused on a delete_lock, e.g. the background).";

export const canvasAgent: Handler = (id, payload, kernel): Json => {
  switch (str(payload, "type")) {
    case "place": {
      const kind = str(payload, "kind");
      const hm = PLACEABLE[kind];
      if (hm === undefined) return { error: `canvas: place — kind ${JSON.stringify(kind)} is one of ${Object.keys(PLACEABLE).join(", ")}`, reason: "bad_arg", arg: "kind" };
      if (!num(payload["x"]) || !num(payload["y"])) return { error: "canvas: place needs x and y (numbers, canvas coordinates)", reason: "bad_arg", arg: "x" };
      const backend = str(payload, "backend_id");
      if ((kind === "terminal" || kind === "chat") && !backend) {
        return { error: `canvas: place ${kind} needs backend_id — the host agent it shows`, reason: "bad_arg", arg: "backend_id" };
      }
      const want = str(payload, "id");
      const aid = want || `${kind}_${Math.random().toString(16).slice(2, 10)}`;
      if (kernel.get(aid) !== undefined) return { error: `canvas: place — ${aid} already exists`, reason: "exists", arg: "id" };
      const meta: Record<string, unknown> = {};
      for (const k of SETTABLE) if (payload[k] !== undefined) meta[k] = payload[k];
      // the cell's BODY is `bytes` (verbatim): html for html, the source for gl
      const body = bodyOf(kind, payload);
      if (typeof body === "object") return body as unknown as Json;
      if (body !== undefined) meta[kind === "gl" ? "gl_source" : "html"] = body;
      if (backend) Object.assign(meta, { backend_id: backend, owns_backend: false });
      // the view's OWN declared grant onto the agent it shows
      if (kind === "terminal") meta["can_call"] = { ...terminalView.grants(backend) };
      if (kind === "chat") meta["can_call"] = { ...aiView.grants(backend) };
      kernel.register(new Agent({ id: aid, parentId: id, handlerModule: hm, meta: meta as Payload }));
      return { placed: aid, kind };
    }
    case "move": {
      const target = str(payload, "id");
      if (kernel.get(target)?.parentId !== id) return { error: `canvas: move — ${JSON.stringify(target)} is not a cell of this canvas`, reason: "not_found", arg: "id" };
      const m: Record<string, number> = {};
      for (const k of ["x", "y", "width", "height"] as const) if (num(payload[k])) m[k] = payload[k] as number;
      if (Object.keys(m).length === 0) return { error: "canvas: move needs x, y, width or height", reason: "bad_arg", arg: "x" };
      kernel.updateMeta(target, m as unknown as Payload);
      return { moved: target, ...m };
    }
    case "update": {
      const target = str(payload, "id");
      if (kernel.get(target)?.parentId !== id) return { error: `canvas: update — ${JSON.stringify(target)} is not a cell of this canvas`, reason: "not_found", arg: "id" };
      const fields = payload["fields"] ?? {};
      if (typeof fields !== "object" || Array.isArray(fields)) return { error: "canvas: update — fields must be {…}", reason: "bad_arg", arg: "fields" };
      if (Object.keys(fields).length === 0 && payload["bytes"] === undefined) return { error: "canvas: update needs bytes (a new body) or fields", reason: "bad_arg", arg: "fields" };
      const bad = Object.keys(fields).filter((k) => !(SETTABLE as readonly string[]).includes(k));
      if (bad.length) return { error: `canvas: update — not settable: ${bad.join(", ")} (settable: ${SETTABLE.join(", ")})`, reason: "bad_arg", arg: "fields" };
      const f = { ...(fields as Record<string, unknown>) };
      const kindOf = kernel.get(target)?.record().handler_module === "gl_agent.ts" ? "gl" : "html";
      const body = bodyOf(kindOf, payload);
      if (typeof body === "object") return body as unknown as Json;
      if (body !== undefined) f[kindOf === "gl" ? "gl_source" : "html"] = body;
      for (const k of ["html", "gl_source"]) {
        if (f[k] !== undefined && typeof f[k] !== "string") return { error: `canvas: update — ${k} must be a string`, reason: "bad_arg", arg: "bytes", hint: "send the markup as <bytes>…</bytes> — verbatim, nothing escaped" };
      }
      kernel.updateMeta(target, f as Payload);
      if ("html" in f) kernel.emit(target, { type: "reload_html" });
      return { updated: target, fields: Object.keys(fields) };
    }
    case "wire": {
      // a GRANT between two cells, both sides: `sender` may call `receiver`
      // with `verbs`. (Not from/to: `to` is ai_core's routing tag.)
      const [from, to] = [str(payload, "sender"), str(payload, "receiver")];
      const verbs = payload["verbs"];
      for (const [k, v] of [["sender", from], ["receiver", to]] as const) {
        if (kernel.get(v)?.parentId !== id) return { error: `canvas: wire — ${k} ${JSON.stringify(v)} is not a cell of this canvas`, reason: "not_found", arg: k };
      }
      if (!Array.isArray(verbs) || verbs.length === 0 || !verbs.every((x) => typeof x === "string")) {
        return { error: "canvas: wire needs verbs: [\"emit\", …]", reason: "bad_arg", arg: "verbs" };
      }
      const merge = (aid: string, field: string, key: string): void => {
        const cur = ((kernel.get(aid)?.meta[field] ?? {}) as Record<string, unknown>);
        const had = Array.isArray(cur[key]) ? (cur[key] as string[]) : [];
        kernel.updateMeta(aid, { [field]: { ...cur, [key]: [...new Set([...had, ...(verbs as string[])])] } } as unknown as Payload);
      };
      merge(from, "can_call", to);
      merge(to, "callable_by", from);
      return { wired: { sender: from, receiver: to, verbs } };
    }
    case "remove": {
      const target = str(payload, "id");
      if (kernel.get(target)?.parentId !== id) return { error: `canvas: remove — ${JSON.stringify(target)} is not a cell of this canvas`, reason: "not_found", arg: "id" };
      return kernel.remove(target) as unknown as Json;
    }
    case "around": {
      const [x, y, radius] = [payload["x"], payload["y"], payload["radius"]];
      const bad = (["x", "y", "radius"] as const).filter((k) => typeof payload[k] !== "number" || !Number.isFinite(payload[k] as number));
      if (bad.length > 0 || (radius as number) <= 0) {
        return {
          error: `canvas: around needs x, y and radius as numbers (radius > 0)${bad.length ? ` — missing or not a number: ${bad.join(", ")}` : ""}`,
          reason: "bad_arg",
          arg: bad[0] ?? "radius",
        };
      }
      if (placed === null) return { error: "canvas: around — the canvas is not rendered (no page has it open)", reason: "not_found" };
      return { members: around(placed(), x as number, y as number, radius as number) } as unknown as Json;
    }
    case "boot":
    case "shutdown":
      return null;
    default:
      return { error: `canvas: unknown verb '${str(payload, "type")}'`, reason: "unhandled" };
  }
};

/** Register the canvas root's bundle. */
export function registerCanvas(kernel: {
  registerBundle(hm: string, h: Handler): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(CANVAS, canvasAgent);
  kernel.setBundleReadme(CANVAS, CANVAS_README);
}
