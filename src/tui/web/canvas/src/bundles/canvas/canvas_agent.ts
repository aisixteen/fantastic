import { Agent, str } from "../../lib.ts";
import type { Handler, Json, Payload } from "../../lib.ts";
import { terminalView } from "../terminal_view/terminal_view.ts";
import { bodyText } from "../body_text.ts";
import { aiView } from "../ai_view/ai_view.ts";
import { htmlAgent } from "../html_agent/html_agent.ts";

// THE CANVAS AGENT — the frontend root's own verbs: the CONTROL PLANE over its
// own cells (owner
// 2026-09-30: an AI cell is the canvas's control plane) — `place`, `move`,
// `update`, `wire`, `remove`. `around` is the HOST's (`users.around`, from the
// page records — no tab needs to show the canvas). The page kernel has no
// tree-CRUD verbs by design;
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
  // a CHILD CANVAS: a container of its own; on this canvas it is a cell showing
  // its card html, which the child rewrites itself (`set_html` on it)
  canvas: "canvas.ts",
};

/** Fields `place` / `update` may set — never identity, placement in the tree,
 *  grants or a view's binding. */
export const SETTABLE = ["display_name", "html", "gl_source", "trusted", "readme", "x", "y", "width", "height"] as const;

/** What `update` may set on the CANVAS ITSELF (`id` = the canvas): its MAIN
 *  chat — the canvas's one voice, the AI other canvases talk to (a chat cell of
 *  this canvas, or null for none). Groundwork for specialist canvases
 *  (tmp/specialist_canvases.md); nothing else reads it yet. */
export const CANVAS_SETTABLE = ["main_ai_chat"] as const;

/** A child canvas's first card on its parent — the child rewrites it. */
export const CARD_HTML =
  "<div style='font:14px system-ui;color:#cde;padding:14px;line-height:1.5'>" +
  "<b>a child canvas</b><br>open it with ↗ above, or from the tree.<br>" +
  "<span style='opacity:.6'>This card is the child's own: it rewrites it (set_html on the child canvas) as its control panel.</span></div>";

/** Every canvas verb, for the grants that admit a caller. */
export const CANVAS_VERBS = ["place", "move", "update", "wire", "remove", "reflect", "get_html", "set_html"] as const;

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


export const CANVAS_README =
  "Canvas compositor (frontend root). Renders the frontend's own member tree; mounts each member inline via its view bundle, or as an iframe for external content. Not a host client. Its BACKGROUND is its own singleton child agent, `background` (set_bg / clear_bg / get_bg there). WHAT IS NEAR A POINT is the host's: `around {canvas?, x, y, radius}` on its `users` gateway (canvas coordinates, x grows right, y grows down) answers from the page records, nearest first — reflect a member for more. THE CONTROL PLANE over its cells: `place {kind: html|gl|terminal|chat|canvas, x, y, id?, width?, height?, display_name?, bytes? (the cell's body, VERBATIM: html for html, the GL source for gl, the CARD for a canvas), trusted?, backend_id? (terminal/chat: the host agent shown)}` → {placed}; a `canvas` is a CHILD CANVAS — a container with its own cells, verbs and background, nested under this one; with `template: <a template's name>` it comes WITH its main chat (a kept AI cell from that template, made main) → {placed, main_chat}; on this canvas it shows its CARD, html the child owns: `get_html` / `set_html {bytes}` on the child canvas rewrite it (its control panel here); `move {id, x?, y?, width?, height?}`; `update {id, bytes? (the new body, verbatim), fields?: {display_name|trusted|readme|x|y|width|height}}` (a new html body reloads the panel); `update {id: <this canvas>, fields: {main_ai_chat: <a chat cell's id> | null}}` sets the canvas's MAIN chat — its one voice, named on its record (reflect the canvas to find it); the main chat is delete-locked while main, and making another main (or null) unlocks it; `wire {sender, receiver, verbs}` (a grant between two cells, both sides — e.g. verbs [emit] lets the sender's panel emit to the receiver's); `remove {id}` (refused on a delete_lock, e.g. the background).";

// THE SCAFFOLD (specialist canvases, owner 2026-10-02/03): a child canvas placed
// WITH a `template` comes with its MAIN chat — a kept AI cell from that
// template, made main. The page (`canvas.ts`) hands the spawning here once it
// is mounted; no default template: whoever scaffolds names it.
let scaffold: ((canvas: string, template: string) => Promise<Json>) | null = null;
export function setScaffold(fn: (canvas: string, template: string) => Promise<Json>): void {
  scaffold = fn;
}

export const canvasAgent: Handler = (id, payload, kernel): Json | Promise<Json> => {
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
      const template = payload["template"];
      if (template !== undefined && (kind !== "canvas" || typeof template !== "string" || template === "")) {
        return { error: "canvas: place — `template` (a template's name) is for kind canvas: its main chat", reason: "bad_arg", arg: "template" };
      }
      if (typeof template === "string" && scaffold === null) {
        return { error: "canvas: place — this page cannot scaffold a main chat (no page is mounted)", reason: "not_found", arg: "template" };
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
      if (kind === "canvas" && meta["html"] === undefined) meta["html"] = CARD_HTML;
      // a cell carries its size from birth: the host answers `around` from the records
      meta["width"] ??= kind === "canvas" ? 340 : 480;
      meta["height"] ??= kind === "canvas" ? 220 : 320;
      if (backend) Object.assign(meta, { backend_id: backend, owns_backend: false });
      // the view's OWN declared grant onto the agent it shows
      if (kind === "terminal") meta["can_call"] = { ...terminalView.grants(backend) };
      if (kind === "chat") meta["can_call"] = { ...aiView.grants(backend) };
      kernel.register(new Agent({ id: aid, parentId: id, handlerModule: hm, meta: meta as Payload }));
      if (typeof template === "string" && scaffold !== null) {
        return scaffold(aid, template).then((main) => ({ placed: aid, kind, main_chat: main }) as unknown as Json);
      }
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
      if (target === id) return updateSelf(id, payload, kernel);
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
      const out = kernel.remove(target) as unknown as Json;
      // the main chat gone, the canvas has no voice
      if (kernel.get(target) === undefined && kernel.get(id)?.meta["main_ai_chat"] === target) {
        kernel.updateMeta(id, { main_ai_chat: null } as unknown as Payload);
      }
      return out;
    }
    // THE CARD: this canvas's presence on its parent — html it owns and rewrites
    case "get_html":
    case "render_html":
    case "set_html": {
      return htmlAgent(id, payload, kernel) as Json; // html_agent answers synchronously
    }
    case "boot":
    case "shutdown":
      return null;
    default:
      return { error: `canvas: unknown verb '${str(payload, "type")}'`, reason: "unhandled" };
  }
};

/** `update` on the canvas itself: only [`CANVAS_SETTABLE`]. */
function updateSelf(id: string, payload: Payload, kernel: Parameters<Handler>[2]): Json {
  const fields = payload["fields"];
  if (typeof fields !== "object" || fields === null || Array.isArray(fields) || Object.keys(fields).length === 0) {
    return { error: `canvas: update on the canvas itself needs fields: {${CANVAS_SETTABLE.join("|")}}`, reason: "bad_arg", arg: "fields" };
  }
  const f = fields as Record<string, unknown>;
  const bad = Object.keys(f).filter((k) => !(CANVAS_SETTABLE as readonly string[]).includes(k));
  if (bad.length) return { error: `canvas: update — not settable on the canvas: ${bad.join(", ")} (settable: ${CANVAS_SETTABLE.join(", ")})`, reason: "bad_arg", arg: "fields" };
  const main = f["main_ai_chat"];
  if (main !== null) {
    const cell = typeof main === "string" ? kernel.get(main) : undefined;
    if (cell === undefined || cell.parentId !== id || cell.record().handler_module !== PLACEABLE["chat"]) {
      return { error: `canvas: main_ai_chat must name a chat cell of this canvas, or null — ${JSON.stringify(main)} is none`, reason: "bad_arg", arg: "main_ai_chat" };
    }
  }
  // THE MAIN CHAT IS DELETE-LOCKED while main: the previous one unlocked, the
  // new one locked (`remove` refuses it until it is unmade main)
  const prev = kernel.get(id)?.meta["main_ai_chat"];
  if (typeof prev === "string" && prev !== main && kernel.get(prev) !== undefined) {
    kernel.updateMeta(prev, { delete_lock: false } as unknown as Payload);
  }
  if (typeof main === "string") kernel.updateMeta(main, { delete_lock: true } as unknown as Payload);
  kernel.updateMeta(id, { main_ai_chat: main } as unknown as Payload);
  return { updated: id, fields: ["main_ai_chat"] };
}

/** Register the canvas root's bundle. */
export function registerCanvas(kernel: {
  registerBundle(hm: string, h: Handler): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(CANVAS, canvasAgent);
  kernel.setBundleReadme(CANVAS, CANVAS_README);
}
