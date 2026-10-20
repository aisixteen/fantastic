import type { ViewHandle } from "../view.ts";
import { AgentIndex } from "./agent_index.ts";
import type { AgentWalker } from "./agent_walk.ts";

// WHAT THE FILE TREE AND THE PREVIEWER NEED FROM THE CANVAS (handed over at
// mount by `canvas.ts`, like the canvas-list's deps), and what the cells share
// across the page: the agent index, the selection, and the "draw again" call.

export type Rec = Record<string, unknown>;

export interface FilesDeps {
  /** this canvas's host agents, the registry synced first */
  hostAgents(): Promise<Rec[]>;
  /** the same, as the page has them now (no host round trip) */
  hostRecords(): Rec[];
  /** every agent of the page's own kernel (records) */
  pageAgents(): Rec[];
  /** host agent `id` is one of THIS canvas's (page agents: the page kernel holds it) */
  seenHere(id: string): boolean;
  /** host agent `id` exists on the host at all (another canvas's, perhaps) */
  onHost(id: string): boolean;
  /** the cells showing agent `id` (the cell itself, or cells fronting it) */
  cellsFor(id: string): string[];
  moveTo(viewId: string): void;
  /** VIEW a page agent (`focus`: ⌘-click — focus it on its view instead) */
  view(id: string, focus: boolean): void;
  /** the page agent viewed now */
  viewed(): string;
  /** macOS: ⌘ is the focus modifier (Ctrl-click is the Mac's right click) */
  mac: boolean;
  /** open host agent `id` in a new cell, drawn by its own view */
  open(id: string): void;
  /** a host agent added in the background and not shown yet (NEW) */
  fresh(id: string): boolean;
  freshIds(): string[];
  /** delete host agent `id` (its own act: ✕ on a cell only closes the view) */
  deleteAgent(id: string): void;
  /** agent `agentId`'s own live view, inside `el` (host: its view bundle; page: a reflect card) */
  mountAgentView(agentId: string, el: HTMLElement, selfId: string): Promise<ViewHandle>;
}

let deps: FilesDeps | null = null;

/** The canvas hands its wiring over, at mount. */
export function setFilesDeps(d: FilesDeps): void {
  deps = d;
}

export function filesDeps(who: string): FilesDeps {
  if (deps === null) throw new Error(`${who}: the canvas is not mounted`);
  return deps;
}

/** What the tree shows of an agent: its folder on disk is the agent. The project's
 *  folder ↔ id map, read off every `agent.yaml` under `.fantastic`. */
export const agentIndex = new AgentIndex();

// ─── WALKING THE AGENT FOLDERS ──────────────────────────────────────────────

let walker: AgentWalker | null = null;
export const getWalker = (): AgentWalker | null => walker;
export const setWalker = (w: AgentWalker): void => {
  walker = w;
};

/** Ask for the agent folders to be read again (coalesced; a no-op before any tree is up). */
export function walkAgain(): void {
  void walker?.request();
}

// ─── THE SELECTION BUS ──────────────────────────────────────────────────────

/** What the tree has selected: a path, or (an agent with no folder on disk) just its id. */
export interface Selection {
  path: string | null;
  agent: string | null;
}

let current: Selection | null = null;
const listeners = new Set<(s: Selection | null, from: string) => void>();

/** The tree `from` selects `s`: every following preview shows it. */
export function select(s: Selection | null, from: string): void {
  current = s;
  for (const fn of [...listeners]) fn(s, from);
}

export const selection = (): Selection | null => current;

export function onSelection(fn: (s: Selection | null, from: string) => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

// ─── DRAWING AGAIN ──────────────────────────────────────────────────────────

const drawers = new Set<() => void>();
let lastIds = "";

export function onRender(fn: () => void): () => void {
  drawers.add(fn);
  return () => drawers.delete(fn);
}

/** The root of a record set: the first (by id) whose parent is not in it. */
export function rootOf(recs: Rec[]): string {
  const ids = new Set(recs.map((r) => String(r["id"])));
  const roots = recs.filter((r) => typeof r["parent_id"] !== "string" || !ids.has(r["parent_id"])).map((r) => String(r["id"]));
  return roots.sort()[0] ?? "";
}

/** Draw every tree and preview again from what they hold. */
export function drawAll(): void {
  for (const fn of [...drawers]) fn();
}

/** The canvas says something changed: draw every tree again (decorations only — no
 *  re-listing), and read the agent folders again when the set of agents changed. */
export function renderFileTrees(): void {
  if (deps !== null) {
    const ids = [...deps.hostRecords(), ...deps.pageAgents()].map((r) => String(r["id"])).sort().join("\n");
    if (ids !== lastIds) {
      lastIds = ids;
      walkAgain();
    }
  }
  drawAll();
}
