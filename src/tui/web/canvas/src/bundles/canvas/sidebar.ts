// THE SIDEBAR — a CELL like any other (owner 2026-10-08): placed from the
// double-click, moved, resized, closed; as many as you like. Nothing on the
// canvas floats: a canvas opens as its background alone.
//
// It is JUST THE TREE: one per kernel the page sees — the host kernel (this
// canvas's agents) and the page's own kernel (the canvas and its views).
//
// A click on a PAGE agent VIEWS it (`viewer.ts`: a view → that view, any
// other → it full); ⌘-click (Ctrl-click off macOS) FOCUSES it instead — its
// view viewed, the cell centred and flashed. A host agent a cell shows: click
// views that cell, ⌘-click moves to it (again → the next such cell). An agent
// no cell shows OPENS in a new cell: an AI as a chat, anything else as its
// reflect card.

import { isFocusClick } from "./viewer.ts";

type Rec = Record<string, unknown>;
export interface TreeNode {
  id: string;
  handler: string;
  rec: Rec;
  children: TreeNode[];
}

export interface SidebarDeps {
  mount: HTMLElement;
  /** every host agent (records with `parent_id`), paged to the end */
  hostAgents(): Promise<Rec[]>;
  /** the page kernel's tree, rooted at the canvas */
  pageTree(): TreeNode | null;
  /** the cells showing agent `id` (the cell itself, or cells fronting it) */
  cellsFor(id: string): string[];
  /** whether cell `id` is hidden from its view (#133: its agent runs on) */
  isHidden(cell: string): boolean;
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
  /** delete host agent `id` (its own act: ✕ on a cell only hides it) */
  deleteAgent(id: string): void;
}

const str = (r: Rec, k: string): string | undefined => (typeof r[k] === "string" ? (r[k] as string) : undefined);

/** Whether `n`'s subtree (below it) holds agent `id`. */
export function holds(n: TreeNode, id: string): boolean {
  return n.children.some((c) => c.id === id || holds(c, id));
}

/** Build the host's forest from flat records (parent_id). Pure, testable. */
export function forest(recs: Rec[]): TreeNode[] {
  const nodes = new Map<string, TreeNode>();
  for (const r of recs) {
    const id = str(r, "id");
    if (id !== undefined) nodes.set(id, { id, handler: str(r, "handler_module") ?? "", rec: r, children: [] });
  }
  const roots: TreeNode[] = [];
  for (const n of nodes.values()) {
    const p = str(n.rec, "parent_id");
    const parent = p !== undefined ? nodes.get(p) : undefined;
    if (parent) parent.children.push(n);
    else roots.push(n);
  }
  const sort = (xs: TreeNode[]): void => {
    xs.sort((a, b) => a.id.localeCompare(b.id));
    for (const x of xs) sort(x.children);
  };
  sort(roots);
  return roots;
}

/** Whether an agent's cells are ALL hidden (#133): it runs, and no cell shows it.
 *  A row with no cell at all is not hidden — it is simply not shown. Pure, testable. */
export function allHidden(cells: string[], isHidden: (cell: string) => boolean): boolean {
  return cells.length > 0 && cells.every(isHidden);
}

/** The glyph for an agent, by what its record says it is. An AI agent is a
 *  NON-ROOT agent naming a memory store (the root names the project's). */
export function glyph(n: TreeNode): string {
  if (typeof n.rec["memory_store_id"] === "string" && typeof n.rec["parent_id"] === "string") return "✦";
  if (n.handler.startsWith("exec_bridge")) return "⌨";
  if (n.handler.includes("store")) return "▣";
  if (n.handler.endsWith(".ts")) return "◻";
  return "◇";
}

export function createSidebar(d: SidebarDeps): { render(): Promise<void>; el: HTMLElement } {
  const el = document.createElement("div");
  el.className = "side";
  // just the tree (owner 2026-10-08: no chats list, no headings)
  el.innerHTML = `<ul class="agents-tree"></ul>`;
  d.mount.appendChild(el);
  const tree = el.querySelector(".agents-tree") as HTMLElement;

  // several cells on one thing: each click moves to the next one
  const cycle = new Map<string, number>();
  const moveNext = (key: string, cells: string[]): void => {
    const i = (cycle.get(key) ?? -1) + 1;
    cycle.set(key, i);
    d.moveTo(cells[i % cells.length]!);
  };
  // the user's own expand / collapse choices; unset = open to depth 1
  const open = new Map<string, boolean>();

  const node = (n: TreeNode, depth: number, out: HTMLElement[]): void => {
    const cells = d.cellsFor(n.id);
    // hidden (#133): every cell of it is off its view — shown, it is not "has-cell"
    const hidden = allHidden(cells, d.isHidden);
    const shown = hidden ? [] : cells;
    const kids = n.children.length > 0;
    // the path to the VIEWED agent is always unfolded: you see where you are
    const isOpen = open.get(n.id) ?? (depth < 1 || n.id === d.viewed() || holds(n, d.viewed()));
    const li = document.createElement("li");
    // FE (page agents, `*.ts`) and BE (host agents) in the canvas's two colors;
    // a host agent added in the background and not shown yet is NEW
    const side = n.handler.endsWith(".ts") ? "page" : "host";
    const isNew = side === "host" && d.fresh(n.id);
    li.className = "agent " + side + (shown.length > 0 ? " has-cell" : "") + (hidden ? " hidden" : "") + (n.id === d.viewed() ? " viewed" : "") + (isNew ? " fresh" : "");
    li.style.paddingLeft = `${10 + depth * 12}px`;
    li.dataset.id = n.id;
    li.innerHTML = `<span class="tw"></span><span class="gl"></span><span class="nm"></span><span class="ct"></span>`;
    const tw = li.children[0] as HTMLElement;
    tw.textContent = kids ? (isOpen ? "▾" : "▸") : "";
    tw.addEventListener("click", (e) => {
      e.stopPropagation();
      open.set(n.id, !isOpen);
      void renderTree();
    });
    (li.children[1] as HTMLElement).textContent = glyph(n);
    (li.children[2] as HTMLElement).textContent = n.id;
    (li.children[3] as HTMLElement).textContent = hidden ? "hidden" : isNew ? "new" : cells.length > 1 ? String(cells.length) : "";
    const fkey = d.mac ? "⌘" : "Ctrl";
    li.title = n.handler.endsWith(".ts")
      ? `${n.id} · ${n.handler} · click: view it · ${fkey}-click: focus it on its canvas`
      : hidden
        ? `${n.id} · ${n.handler} · hidden: click shows its cell and views it · ${fkey}-click: show it on its view`
        : `${n.id} · ${n.handler}${cells.length > 0 ? ` · click: view its cell · ${fkey}-click: move to it` : " · open it in a cell"}`;
    li.addEventListener("click", (e) => {
      const focus = isFocusClick(e, d.mac);
      if (n.handler.endsWith(".ts")) return d.view(n.id, focus); // a page agent
      const now = d.cellsFor(n.id);
      if (now.length > 0) {
        if (focus) moveNext(n.id, now);
        else d.view(now[0]!, false);
      } else if (n.handler !== "") d.open(n.id);
    });
    if (side === "host" && n.handler !== "") {
      const del = document.createElement("span");
      del.className = "del";
      del.textContent = "⌫";
      del.title = `delete ${n.id} (the agent, not just its view)`;
      del.addEventListener("click", (e) => {
        e.stopPropagation();
        d.deleteAgent(n.id);
      });
      li.appendChild(del);
    }
    out.push(li);
    if (kids && isOpen) for (const c of n.children) node(c, depth + 1, out);
  };

  const kernelRow = (label: string, out: HTMLElement[]): void => {
    const li = document.createElement("li");
    li.className = "kernel";
    li.textContent = label;
    out.push(li);
  };

  const renderTree = async (): Promise<void> => {
    const out: HTMLElement[] = [];
    const hosts = forest(await d.hostAgents().catch(() => [] as Rec[]));
    kernelRow(`◆ host${hosts[0] ? ` · ${hosts[0].id}` : ""}`, out);
    for (const r of hosts) node(r, 0, out);
    const page = d.pageTree();
    kernelRow(`◆ page${page ? ` · ${page.id}` : ""}`, out);
    if (page) node(page, 0, out);
    tree.replaceChildren(...out);
  };

  return {
    async render(): Promise<void> {
      await renderTree();
    },
    el,
  };
}
