// BACKEND-OWNED VIEWS (tmp/backend_owned_views.md): the HOST agent is the unit,
// its record declares how it is shown, and a page cell is only a VIEW of it —
// layout (where, how big) plus `backend_id`, the agent it shows. This module is
// the pure part: a host record → the view it gets, the page bundle that draws
// it, and the cell's grant onto its agent. Nothing here touches the DOM or the
// kernels.
//
//   view: {kind: terminal|chat|html|…  OR  handler: <any page bundle>,
//          bytes | bytes_file   (html: the body — the agent's own small renderer),
//          calls: [verbs…]      (html: what the body calls on its agent)}
//
// No `view` → the agent kind's DEFAULT (an exec agent → terminal, an AI → chat),
// else the STUB (its reflect + docs). One view per agent.

export type Rec = Record<string, unknown>;

/** What a host agent is drawn with. */
export interface HostView {
  /** the page bundle that renders it */
  handler: string;
  /** the view kind it came from (`terminal`, `chat`, `html`, `stub`, or `handler`) */
  kind: string;
  /** html: the body, when inline */
  bytes?: string;
  /** html: the body's file, when not inline (read through the file leg) */
  bytesFile?: string;
  /** the verbs the view calls on its agent (html: the record's; else the renderer's) */
  calls: string[];
}

/** The page renderers the generic kinds name. */
export const RENDERERS: Record<string, string> = {
  terminal: "terminal_view.ts",
  chat: "ai_view.ts",
  html: "agent_view.ts",
  stub: "reflect_view.ts",
  mcp: "mcp_view.ts",
};

/** The fixed grants a generic renderer needs on the agent it shows. */
const RENDERER_CALLS: Record<string, string[]> = {
  terminal: ["watch", "status", "start", "write", "ack", "resize", "output", "paste_image"],
  chat: ["watch", "reflect", "history", "status", "send", "stop"],
  stub: ["reflect"],
  mcp: ["reflect", "boot", "watch"],
};

const EXEC = "exec_bridge.tools";
const MCP = "mcp_client.tools";

const str = (r: Rec, k: string): string | undefined => (typeof r[k] === "string" && r[k] !== "" ? (r[k] as string) : undefined);

/** An AI agent: the field that makes one is its memory store (`memory_store_id`). */
const isAi = (r: Rec): boolean => str(r, "memory_store_id") !== undefined && str(r, "handler_module") !== undefined;

/** The view a host record gets: its own `view`, else its kind's default, else the stub. */
export function viewOf(r: Rec): HostView {
  const v = r["view"] !== null && typeof r["view"] === "object" ? (r["view"] as Rec) : null;
  if (v !== null) {
    const handler = str(v, "handler");
    const kind = str(v, "kind") ?? (handler !== undefined ? "handler" : "stub");
    const calls = Array.isArray(v["calls"]) ? (v["calls"] as unknown[]).filter((c): c is string => typeof c === "string") : [];
    const out: HostView = {
      handler: handler !== undefined ? (handler.endsWith(".ts") ? handler : `${handler}.ts`) : (RENDERERS[kind] ?? RENDERERS["stub"]!),
      kind,
      calls: kind === "html" || kind === "handler" ? withWatch(calls) : (RENDERER_CALLS[kind] ?? withWatch(calls)),
    };
    if (str(v, "bytes") !== undefined) out.bytes = str(v, "bytes");
    if (str(v, "bytes_file") !== undefined) out.bytesFile = str(v, "bytes_file");
    return out;
  }
  if (str(r, "handler_module") === EXEC) return { handler: RENDERERS["terminal"]!, kind: "terminal", calls: RENDERER_CALLS["terminal"]! };
  if (str(r, "handler_module") === MCP) return { handler: RENDERERS["mcp"]!, kind: "mcp", calls: RENDERER_CALLS["mcp"]! };
  if (isAi(r)) return { handler: RENDERERS["chat"]!, kind: "chat", calls: RENDERER_CALLS["chat"]! };
  return { handler: RENDERERS["stub"]!, kind: "stub", calls: RENDERER_CALLS["stub"]! };
}

/** A view always receives its agent's events. */
function withWatch(calls: string[]): string[] {
  return calls.includes("watch") ? calls : [...calls, "watch"];
}

/** The cell's OWN grant onto the agent it shows — derived, never hand-written. */
export function cellGrant(agentId: string, view: HostView): Record<string, string[]> {
  return { [agentId]: [...view.calls] };
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
