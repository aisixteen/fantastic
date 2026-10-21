import { inRoot, norm } from "./paths.ts";

// THE TREE'S DRAG (#134, #135): what a dragged row carries, how rows are marked
// together, where the canvas puts what it is given, and what a target is. Pure: no
// DOM, no kernel. The tree's rows, the canvas's drop and the view's `open` verb all
// read it from here, so a drop and an AI's `open` resolve the same way.

/** The MIME type a tree drag carries: JSON `{items: DragItem[]}`. */
export const DRAG_MIME = "application/x-fantastic-tree";

/** One thing dragged out of the tree: an agent (by id), or a project file or folder (by path). */
export type DragItem = { kind: "agent"; id: string } | { kind: "file" | "dir"; path: string };

export function encodeDrag(items: readonly DragItem[]): string {
  return JSON.stringify({ items });
}

/** The items of a tree drag, or why the payload is not one (never a partial list). */
export function decodeDrag(text: string): { items: DragItem[] } | { error: string } {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return { error: "not a file tree drag: the payload is not JSON" };
  }
  const list = (raw as { items?: unknown } | null)?.items;
  if (!Array.isArray(list)) return { error: "not a file tree drag: no items" };
  const items: DragItem[] = [];
  for (const it of list) {
    const o = (it ?? {}) as Record<string, unknown>;
    if (o["kind"] === "agent" && typeof o["id"] === "string" && o["id"] !== "") items.push({ kind: "agent", id: o["id"] });
    else if ((o["kind"] === "file" || o["kind"] === "dir") && typeof o["path"] === "string") items.push({ kind: o["kind"], path: o["path"] });
    else return { error: `not a file tree item: ${JSON.stringify(it)}` };
  }
  return { items };
}

// ─── SEVERAL ROWS AT ONCE ───────────────────────────────────────────────────

/** The keys from `anchor` to `to`, both included, in `order` (the rows as shown). `to` alone when either is not there. */
export function rangeOf(order: readonly string[], anchor: string, to: string): string[] {
  const a = order.indexOf(anchor);
  const b = order.indexOf(to);
  if (a < 0 || b < 0) return [to];
  const [lo, hi] = a < b ? [a, b] : [b, a];
  return order.slice(lo, hi + 1);
}

/** The marks with `key` toggled: in when it is not, out when it is. Order kept. */
export function toggled(marks: readonly string[], key: string): string[] {
  return marks.includes(key) ? marks.filter((m) => m !== key) : [...marks, key];
}

// ─── WHERE THE CANVAS PUTS WHAT IS DROPPED ──────────────────────────────────

/**
 * Where `n` cells of one size go around a drop point, as their top-left corners:
 * a grid of ceil(√n) columns, row by row, centred on the point. Deterministic, and
 * no two slots overlap (the slot is the cell's size plus the gap).
 */
export function layoutAround(at: { x: number; y: number }, n: number, slot: { width: number; height: number }, gap = 24): Array<{ x: number; y: number }> {
  if (n <= 0) return [];
  const cols = Math.ceil(Math.sqrt(n));
  const rows = Math.ceil(n / cols);
  const gridW = cols * slot.width + (cols - 1) * gap;
  const gridH = rows * slot.height + (rows - 1) * gap;
  const x0 = Math.round(at.x - gridW / 2);
  const y0 = Math.round(at.y - gridH / 2);
  return Array.from({ length: n }, (_, i) => ({
    x: x0 + (i % cols) * (slot.width + gap),
    y: y0 + Math.floor(i / cols) * (slot.height + gap),
  }));
}

// ─── WHAT A DROP OR AN `open` MEANS ─────────────────────────────────────────

/** What is asked for: a drag item, or a path whose kind (file or folder) is looked up on disk. */
export type Target = DragItem | { kind: "any"; path: string };

/** What the target opens: an agent (its cell), a file (a preview pinned to it), a folder (a tree rooted at it). */
export type Opened = { kind: "agent"; agent: string } | { kind: "file"; path: string } | { kind: "dir"; path: string };

/** A target that cannot be opened, said loudly: what, and the argument it came in. */
export interface Refusal {
  error: string;
  reason: "bad_arg" | "outside_root" | "not_found";
  arg: string;
  hint?: string;
}

/** What the canvas can look at: the project on disk, the agent folders, the host's agents. */
export interface Look {
  /** what is at a project path: a file, a folder, or nothing. Throws when the bridge refuses. */
  kindOf(path: string): Promise<"file" | "dir" | undefined>;
  /** the agent whose folder is `path`, if any */
  agentAt(path: string): string | undefined;
  /** host agent `id` exists */
  hasAgent(id: string): Promise<boolean>;
}

const refusal = (error: string, reason: Refusal["reason"], arg: string, hint?: string): Refusal => ({ error, reason, arg, ...(hint !== undefined ? { hint } : {}) });

/**
 * Resolve a target to what it opens. An agent id is an agent; a path is an agent
 * when it is an agent's folder, else a file or a folder by what is on disk. A path
 * that is not there, is outside the project, or is the wrong kind is refused.
 */
export async function resolveTarget(t: Target, look: Look): Promise<Opened | Refusal> {
  if (t.kind === "agent") {
    return (await look.hasAgent(t.id)) ? { kind: "agent", agent: t.id } : refusal(`no host agent '${t.id}'`, "not_found", "agent", "the agents are the host's: drag one from the tree");
  }
  if (!inRoot(t.path)) return refusal(`'${t.path}' is outside the project`, "outside_root", "path", "paths are relative to the project folder, with no leading / and no ..");
  const p = norm(t.path);
  const agent = t.kind === "file" ? undefined : look.agentAt(p);
  if (agent !== undefined) {
    return (await look.hasAgent(agent)) ? { kind: "agent", agent } : refusal(`'${p}' is the folder of agent '${agent}', which is not a host agent`, "not_found", "path");
  }
  const kind = p === "" ? "dir" : await look.kindOf(p);
  if (kind === undefined) return refusal(`'${p}' is not in the project`, "not_found", "path");
  if (t.kind === "file" && kind !== "file") return refusal(`'${p}' is a folder, not a file`, "bad_arg", "path", "drag a folder to open it as a tree");
  if (t.kind === "dir" && kind !== "dir") return refusal(`'${p}' is a file, not a folder`, "bad_arg", "path", "drag a file to open it as a preview");
  return kind === "file" ? { kind: "file", path: p } : { kind: "dir", path: p };
}

/** Whether a resolution is a refusal. */
export const isRefusalOf = (r: Opened | Refusal): r is Refusal => "error" in r;

// ─── WHERE A DROP IS, IN THE CANVAS'S WORLD ─────────────────────────────────

/** The canvas's view: its zoom and the world's pan, in screen pixels. */
export interface ViewPose {
  z: number;
  ox: number;
  oy: number;
}

/**
 * The world point under a screen point, for a canvas whose top-left corner is `rect`
 * and whose world is zoomed `view.z` and panned `(view.ox, view.oy)`. The inverse of
 * the world's transform, so a drop lands at its point at any zoom.
 */
export function worldAt(clientX: number, clientY: number, rect: { left: number; top: number }, view: ViewPose): { x: number; y: number } {
  return {
    x: (clientX - rect.left - view.ox) / view.z,
    y: (clientY - rect.top - view.oy) / view.z,
  };
}

/**
 * The middle of a canvas's box, in client coordinates. The canvas's box is what is left
 * of the viewport beside the file tree (#148), so its middle is not the viewport's.
 */
export function boxCentre(rect: { left: number; top: number; width: number; height: number }): { x: number; y: number } {
  return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
}

/**
 * The view zoomed to `z` about `anchor`, a point in the canvas's own box (its top-left
 * corner is the origin, as the world's offset is): the world point under the anchor stays under it.
 */
export function zoomAbout(view: ViewPose, anchor: { x: number; y: number }, z: number): ViewPose {
  const ratio = z / view.z;
  return { z, ox: anchor.x - (anchor.x - view.ox) * ratio, oy: anchor.y - (anchor.y - view.oy) * ratio };
}

/**
 * Where a drop of `items` at a screen point lands: one spot (the top-left corner of
 * its box, in world coordinates) per item, in the grid `layoutAround` gives, centred
 * on the drop point. The canvas's drop and the tests read it the same way, whatever
 * the drag came from (the tree cell or the sidebar).
 */
export function dropPlan(
  items: readonly DragItem[],
  clientX: number,
  clientY: number,
  rect: { left: number; top: number },
  view: ViewPose,
  slot: { width: number; height: number },
): Array<{ item: DragItem; x: number; y: number }> {
  const spots = layoutAround(worldAt(clientX, clientY, rect, view), items.length, slot);
  return items.map((item, i) => ({ item, x: spots[i]!.x, y: spots[i]!.y }));
}
