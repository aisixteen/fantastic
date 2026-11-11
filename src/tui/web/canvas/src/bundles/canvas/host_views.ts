// BACKEND-OWNED VIEWS (tmp/backend_owned_views.md): the HOST agent is the unit,
// its `reflect` reply declares how it is shown (its record's `view`), and a page cell is only a VIEW of it —
// layout (where, how big) plus `backend_id`, the agent it shows. This module is
// the pure part: a host record → the view it gets, the page bundle that draws
// it, and the cell's grant onto its agent. Nothing here touches the DOM or the
// kernels.
//
//   view: {kind: terminal|html|…  OR  handler: <any page bundle>,
//          bytes                (html: the body — the agent's own small renderer),
//          calls: [verbs…]      (html: what the body calls on its agent)
//                 | {<agent id> | "*": [verbs…]}   (the same, plus READ verbs on other agents)}
//
// No `view` → the agent kind's DEFAULT (an exec agent → terminal, an MCP client, a store…),
// else the STUB (its reflect + docs). One view per agent.

export type Rec = Record<string, unknown>;

/** What a host agent is drawn with. */
export interface HostView {
  /** the page bundle that renders it */
  handler: string;
  /** the view kind it came from (`terminal`, `html`, `stub`, or `handler`) */
  kind: string;
  /** html: the body (the host puts a bundle's view file here) */
  bytes?: string;
  /** the verbs the view calls on its agent (html: the record's; else the renderer's) */
  calls: string[];
  /** html, `calls` given as an object: the verbs on OTHER agents (`{<id> | "*": verbs}`) */
  reach?: Record<string, string[]>;
}

/** The page renderers the generic kinds name. */
export const RENDERERS: Record<string, string> = {
  terminal: "terminal_view.ts",
  html: "agent_view.ts",
  stub: "reflect_view.ts",
  mcp: "mcp_view.ts",
  store: "store_view.ts",
  scheduler: "scheduler_view.ts",
  credentials: "credentials_view.ts",
};

/** The fixed grants a generic renderer needs on the agent it shows. */
const RENDERER_CALLS: Record<string, string[]> = {
  terminal: ["watch", "status", "start", "write", "ack", "resize", "output", "paste_image"],
  stub: ["reflect"],
  mcp: ["reflect", "boot", "watch"],
  // the read-only agent views: reads and `watch`, never a write verb (and the
  // credential view never `get`: a name and a grant, not a value)
  store: ["dump", "search", "watch"],
  scheduler: ["list", "history", "watch"],
  credentials: ["list", "reflect", "watch"],
};

/** The view kind a handler module defaults to, when its record has no `view`. */
export const MODULE_KINDS: Record<string, string> = {
  "ram_store.tools": "store",
  "sqlite_store.tools": "store",
  "scheduler.tools": "scheduler",
  "credential_store.tools": "credentials",
};

const EXEC = "exec_bridge.tools";
const MCP = "mcp_client.tools";

const str = (r: Rec, k: string): string | undefined => (typeof r[k] === "string" && r[k] !== "" ? (r[k] as string) : undefined);

/** The view a host record gets: its own `view`, else its kind's default, else the stub. */
export function viewOf(r: Rec): HostView {
  const v = r["view"] !== null && typeof r["view"] === "object" ? (r["view"] as Rec) : null;
  if (v !== null) {
    const handler = str(v, "handler");
    const kind = str(v, "kind") ?? (handler !== undefined ? "handler" : "stub");
    const strings = (x: unknown): string[] => (x as unknown[]).filter((c): c is string => typeof c === "string");
    const given = v["calls"];
    // a LIST is the verbs on the view's own agent; an OBJECT names its own agent by id and may name others
    const byTarget = given !== null && typeof given === "object" && !Array.isArray(given) ? (given as Rec) : null;
    const own = str(r, "id");
    const calls = Array.isArray(given) ? strings(given) : byTarget !== null && own !== undefined && Array.isArray(byTarget[own]) ? strings(byTarget[own]) : [];
    const reach: Record<string, string[]> = {};
    if (byTarget !== null) for (const [target, verbs] of Object.entries(byTarget)) if (target !== own && Array.isArray(verbs)) reach[target] = strings(verbs);
    const out: HostView = {
      handler: handler !== undefined ? (handler.endsWith(".ts") ? handler : `${handler}.ts`) : (RENDERERS[kind] ?? RENDERERS["stub"]!),
      kind,
      calls: kind === "html" || kind === "handler" ? withWatch(calls) : (RENDERER_CALLS[kind] ?? withWatch(calls)),
    };
    if (Object.keys(reach).length > 0 && (kind === "html" || kind === "handler")) out.reach = reach;
    if (str(v, "bytes") !== undefined) out.bytes = str(v, "bytes");
    return out;
  }
  if (str(r, "handler_module") === EXEC) return { handler: RENDERERS["terminal"]!, kind: "terminal", calls: RENDERER_CALLS["terminal"]! };
  if (str(r, "handler_module") === MCP) return { handler: RENDERERS["mcp"]!, kind: "mcp", calls: RENDERER_CALLS["mcp"]! };
  const module = str(r, "handler_module");
  const byModule = module === undefined ? undefined : MODULE_KINDS[module];
  if (byModule !== undefined) return { handler: RENDERERS[byModule]!, kind: byModule, calls: RENDERER_CALLS[byModule]! };
  return { handler: RENDERERS["stub"]!, kind: "stub", calls: RENDERER_CALLS["stub"]! };
}

/** A view always receives its agent's events. */
function withWatch(calls: string[]): string[] {
  return calls.includes("watch") ? calls : [...calls, "watch"];
}

/** The cell's OWN grant onto the agent it shows — derived, never hand-written — and the
 *  other targets an object `calls` names, as given. Nothing else. */
export function cellGrant(agentId: string, view: HostView): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  for (const [target, verbs] of Object.entries(view.reach ?? {})) out[target] = [...verbs];
  out[agentId] = [...view.calls];
  return out;
}

/** A grant whose `"*"` key (the verbs on every other agent) is written out for each id in
 *  `ids`: the page gate matches ids, not a wildcard. A target the grant names keeps its
 *  verbs, with the `"*"` ones added. */
export function expandReach(grant: Record<string, string[]>, ids: Iterable<string>): Record<string, string[]> {
  const { "*": every, ...named } = grant;
  const out: Record<string, string[]> = {};
  for (const [target, verbs] of Object.entries(named)) out[target] = [...verbs];
  if (every === undefined) return out;
  for (const id of ids) out[id] = [...new Set([...(out[id] ?? []), ...every])];
  return out;
}

/** Whether a page cell is a VIEW of a host agent (BE) or page content (FE). */
export function sideOf(cell: Rec): "host" | "page" {
  return str(cell, "backend_id") !== undefined ? "host" : "page";
}

// ── THE HOST REGISTRY, as the canvas mirrors it (`canvas.ts`) ──────────────
// A host agent's record by id, and a signal when one's `view` changes. Set by
// the canvas at mount; `place` and `agent_view` read it.
let recordOf: (id: string) => Rec | undefined = () => undefined;
const viewChanged = new Map<string, Set<() => void>>();

export function setHostRecords(fn: (id: string) => Rec | undefined): void {
  recordOf = fn;
}
export function hostRecordOf(id: string): Rec | undefined {
  return recordOf(id);
}
// a fresh read of the host's agents (the canvas sets it at mount): an agent made
// a moment ago is not in the page's copy until the next sync
let syncHost: () => Promise<void> = () => Promise.resolve();
export function setHostSync(fn: () => Promise<void>): void {
  syncHost = fn;
}
/** `id`'s host record — read again from the host when the page has none yet. */
export async function hostRecordFresh(id: string): Promise<Rec | undefined> {
  const now = recordOf(id);
  if (now !== undefined) return now;
  await syncHost();
  return recordOf(id);
}
// HOW AN AGENT IS DRAWN is read from its `reflect` reply — the one place any
// client learns it, for a host agent (a Rust/Python backend) and a page agent
// (the page kernel, no backend) alike — never from the `list_agents` record,
// which stays the listing of the tree. The canvas sets the reflector at mount.
let reflectOf: (id: string) => Promise<Rec | undefined> = () => Promise.resolve(undefined);
export function setHostReflect(fn: (id: string) => Promise<Rec | undefined>): void {
  reflectOf = fn;
}
/** `id`'s `reflect` reply, or `undefined` for a refusal (a gate's, or no such
 *  agent): the same refusal any call gets, never a bypass. */
export async function reflectFresh(id: string): Promise<Rec | undefined> {
  const r = await reflectOf(id).catch(() => undefined);
  return r !== undefined && r !== null && typeof r === "object" && typeof r["error"] !== "string" ? r : undefined;
}
/** The record `viewOf` draws `id` from: the listing's fields (kind defaults read
 *  `handler_module`) with the `view` of the agent's REFLECT —
 *  the listing's own `view` is never used. `undefined` when neither the page's
 *  listing nor a reflect knows the agent. */
/** The `view` (JSON) each agent was last drawn or re-reflected with. */
const drawnViews = new Map<string, string>();
export async function viewRecordOf(id: string): Promise<Rec | undefined> {
  const reflected = await reflectFresh(id);
  const listed = recordOf(id);
  if (listed === undefined && reflected === undefined) return undefined;
  const { view: _listed, ...rest } = listed ?? {};
  const out: Rec = { ...rest, ...(reflected ?? {}) };
  if (reflected === undefined || reflected["view"] === undefined) delete out["view"];
  // the first view drawn for an agent is what a later re-reflect compares to
  if (!drawnViews.has(id)) drawnViews.set(id, JSON.stringify(out["view"] ?? null));
  return out;
}
/** `viewRecordOf`, the host's agents read again first when the page has none yet. */
export async function viewRecordFresh(id: string): Promise<Rec | undefined> {
  if (recordOf(id) === undefined) await syncHost();
  return viewRecordOf(id);
}
/** Reflect each agent in `shown` again; one whose `view` differs from the one
 *  it was last drawn with has its cells told (`viewChangedFor`). Answers each
 *  agent's `viewRecordOf`. */
export async function rereflectViews(shown: Set<string>): Promise<Map<string, Rec | undefined>> {
  for (const id of [...drawnViews.keys()]) if (!shown.has(id)) drawnViews.delete(id);
  const was = new Map([...shown].map((id) => [id, drawnViews.get(id)] as const));
  const recs = new Map<string, Rec | undefined>();
  await Promise.all([...shown].map(async (id) => void recs.set(id, await viewRecordOf(id))));
  for (const id of shown) {
    const now = JSON.stringify(recs.get(id)?.["view"] ?? null);
    const before = was.get(id);
    drawnViews.set(id, now);
    if (before !== undefined && before !== now) viewChangedFor(id);
  }
  return recs;
}
/** Run `fn` whenever host agent `id`'s `view` changes; returns the unsubscribe. */
export function onViewChange(id: string, fn: () => void): () => void {
  const set = viewChanged.get(id) ?? new Set<() => void>();
  set.add(fn);
  viewChanged.set(id, set);
  return () => set.delete(fn);
}
/** The canvas says host agent `id`'s `view` changed. */
export function viewChangedFor(id: string): void {
  for (const fn of viewChanged.get(id) ?? []) fn();
}

// ── THIS PAGE'S CANVAS (flat peer canvases, phase 4) ───────────────────────
// The host `canvas.tools` node this page is the view of: its cells' backends
// are its children, and it creates / deletes / spawns them. Set at mount.
let node = "";
export function setCanvasNode(id: string): void {
  node = id;
}
export function canvasNode(): string {
  if (node === "") throw new Error("canvas: no canvas node set (mountCanvas sets it)");
  return node;
}

/** Whether host agent `id` is THIS canvas's to show: in its node's subtree, or
 *  under no canvas at all (the shell's own agents, e.g. the root AI). Another
 *  canvas's agents are not. `parentOf` walks the host records. */
export function seenHere(id: string, parentOf: (id: string) => string | undefined, isCanvas: (id: string) => boolean): boolean {
  let cur: string | undefined = id;
  for (let hops = 0; cur !== undefined && hops < 4096; hops++) {
    if (cur === node) return true;
    if (isCanvas(cur)) return false;
    cur = parentOf(cur);
  }
  return true;
}

/** A cell head's name: its title, and its id beside it only when they differ —
 *  a cell with no display name is titled by its id, shown once. Both are record
 *  fields: the head is built from them as TEXT, never as markup. */
export function headTitle(title: string, id: string): { title: string; id: string | null } {
  return { title, id: title === id ? null : id };
}
