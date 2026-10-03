// THE CANVAS SIDEBAR (left edge, collapsible) — the CLI's sidebar, on the page.
//
//   ✉ activity  every OPEN question to the human (the host's `activity` agent
//           answers; the page never reads rows) — a click goes to the asker —
//           then ★ what reached a canvas's MAIN chat from anyone but the human
//           (its audit trail; the main chat answers it itself).
//   chats   kept dialogs (● — SQLite, the number of cells showing each) and this
//           canvas's ephemeral cells (○ — RAM); `+ new chat here`. The viewed
//           canvas's MAIN chat is pinned first, ★.
//   agents  a TREE per kernel the page sees: the host kernel (every agent
//           `list_agents` shows) and the page's own kernel (the canvas and its
//           views).
//
// A click on a PAGE agent VIEWS it (`viewer.ts`: a canvas → that canvas, any
// other → it full); ⌘-click (Ctrl-click off macOS) FOCUSES it instead — its
// canvas viewed, the cell centred and flashed. A host agent a cell shows: click
// views that cell, ⌘-click moves to it (again → the next such cell). A kept
// chat with no cell open, or ⌥-click on it, opens it in a new cell. An agent
// no cell shows OPENS in a new cell: an AI as a chat, anything else as its
// reflect card.

import { isFocusClick } from "./viewer.ts";

type Rec = Record<string, unknown>;
export interface Dialog {
  id: string;
  name: string;
}
export interface Question {
  /** `main`: a message that reached a main chat (its audit trail), not a question */
  kind?: "main";
  agent: string;
  row: number;
  from: string;
  head: string;
  ts: number;
}
export interface TreeNode {
  id: string;
  handler: string;
  rec: Rec;
  children: TreeNode[];
}

export interface SidebarDeps {
  mount: HTMLElement;
  /** kept dialogs, from ai_cells */
  dialogs(): Promise<Dialog[]>;
  /** this canvas's AI cell records */
  aiCells(): Rec[];
  /** the backend of the VIEWED canvas's main chat (`main_ai_chat`), or null */
  mainChat(): string | null;
  /** every host agent (records with `parent_id`), paged to the end */
  hostAgents(): Promise<Rec[]>;
  /** the page kernel's tree, rooted at the canvas */
  pageTree(): TreeNode | null;
  /** the cells showing agent `id` (the cell itself, or cells fronting it) */
  cellsFor(id: string): string[];
  moveTo(viewId: string): void;
  /** VIEW a page agent (`focus`: ⌘-click — focus it on its canvas instead) */
  view(id: string, focus: boolean): void;
  /** the page agent viewed now */
  viewed(): string;
  /** macOS: ⌘ is the focus modifier (Ctrl-click is the Mac's right click) */
  mac: boolean;
  /** open agent `id` in a new cell (`isAi`: a chat, else its reflect card) */
  open(id: string, isAi: boolean): void;
  /** the open questions to the human */
  activity(): Promise<Question[]>;
  reopen(d: Dialog): void;
  newChat(): void;
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

/** The glyph for an agent, by what its record says it is. An AI agent is a
 *  NON-ROOT agent naming a memory store (the root names the project's). */
export function glyph(n: TreeNode): string {
  if (typeof n.rec["memory_store_id"] === "string" && typeof n.rec["parent_id"] === "string") return "✦";
  if (n.handler.startsWith("exec_bridge")) return "⌨";
  if (n.handler.includes("store")) return "▣";
  if (n.handler.endsWith(".ts")) return "◻";
  return "◇";
}

export function createSidebar(d: SidebarDeps): { render(): Promise<void> } {
  const el = document.createElement("div");
  el.id = "chats";
  el.innerHTML =
    `<div class="chats-head"><span>✉ activity <span class="act-n"></span></span><button class="chats-toggle" title="collapse">«</button></div>` +
    `<ul class="act-list"></ul>` +
    `<div class="chats-head"><span>chats</span></div>` +
    `<ul class="chats-list"></ul><div class="chats-new">+ new chat here</div>` +
    `<div class="chats-head agents-head"><span>agents</span></div><ul class="agents-tree"></ul>`;
  d.mount.appendChild(el);
  const chatsList = el.querySelector(".chats-list") as HTMLElement;
  const actList = el.querySelector(".act-list") as HTMLElement;
  const actN = el.querySelector(".act-n") as HTMLElement;
  const tree = el.querySelector(".agents-tree") as HTMLElement;
  const toggle = el.querySelector(".chats-toggle") as HTMLElement;
  toggle.addEventListener("click", () => {
    el.classList.toggle("collapsed");
    toggle.textContent = el.classList.contains("collapsed") ? "»" : "«";
  });
  (el.querySelector(".chats-new") as HTMLElement).addEventListener("click", () => d.newChat());

  // several cells on one thing: each click moves to the next one
  const cycle = new Map<string, number>();
  const moveNext = (key: string, cells: string[]): void => {
    const i = (cycle.get(key) ?? -1) + 1;
    cycle.set(key, i);
    d.moveTo(cells[i % cells.length]!);
  };
  // the user's own expand / collapse choices; unset = open to depth 1
  const open = new Map<string, boolean>();

  const row = (cls: string, dot: string, name: string, count: string, title: string): HTMLLIElement => {
    const li = document.createElement("li");
    li.className = cls;
    li.innerHTML = `<span class="dot"></span><span class="nm"></span><span class="ct"></span>`;
    (li.children[0] as HTMLElement).textContent = dot;
    (li.children[1] as HTMLElement).textContent = name;
    (li.children[2] as HTMLElement).textContent = count;
    li.title = title;
    return li;
  };

  const renderChats = async (): Promise<void> => {
    const kept = await d.dialogs().catch(() => [] as Dialog[]);
    const cells = d.aiCells();
    const items: HTMLElement[] = [];
    for (const k of kept) {
      const n = cells.filter((c) => c["backend_id"] === k.id).length;
      const li = row("kept", "●", k.name, n > 0 ? String(n) : "", n > 0 ? "move to it · ⌥-click: open another cell" : "open in a new cell");
      li.dataset["backend"] = k.id;
      li.addEventListener("click", (e) => {
        const showing = d.aiCells().filter((c) => c["backend_id"] === k.id).map((c) => String(c["id"]));
        if (showing.length > 0 && !e.altKey) moveNext(k.id, showing);
        else d.reopen(k);
      });
      items.push(li);
    }
    for (const c of cells.filter((c) => c["kept"] !== true && typeof c["backend_id"] === "string")) {
      const li = row("eph", "○", String(c["backend_id"]), "1", "move to it");
      li.addEventListener("click", () => d.moveTo(String(c["id"])));
      items.push(li);
    }
    // the viewed canvas's main chat: pinned first, ★
    const main = d.mainChat();
    const at = main === null ? -1 : items.findIndex((li) => li.dataset["backend"] === main);
    if (at >= 0) {
      const [li] = items.splice(at, 1);
      li!.classList.add("main");
      (li!.children[0] as HTMLElement).textContent = "★";
      li!.title = `this canvas's main chat · ${li!.title}`;
      items.unshift(li!);
    }
    chatsList.replaceChildren(...items);
  };

  const node = (n: TreeNode, depth: number, out: HTMLElement[]): void => {
    const cells = d.cellsFor(n.id);
    const kids = n.children.length > 0;
    // the path to the VIEWED agent is always unfolded: you see where you are
    const isOpen = open.get(n.id) ?? (depth < 1 || n.id === d.viewed() || holds(n, d.viewed()));
    const li = document.createElement("li");
    li.className = "agent" + (cells.length > 0 ? " has-cell" : "") + (n.id === d.viewed() ? " viewed" : "");
    li.style.paddingLeft = `${10 + depth * 12}px`;
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
    (li.children[3] as HTMLElement).textContent = cells.length > 1 ? String(cells.length) : "";
    const fkey = d.mac ? "⌘" : "Ctrl";
    li.title = n.handler.endsWith(".ts")
      ? `${n.id} · ${n.handler} · click: view it · ${fkey}-click: focus it on its canvas`
      : `${n.id} · ${n.handler}${cells.length > 0 ? ` · click: view its cell · ${fkey}-click: move to it` : " · open it in a cell"}`;
    li.addEventListener("click", (e) => {
      const focus = isFocusClick(e, d.mac);
      if (n.handler.endsWith(".ts")) return d.view(n.id, focus); // a page agent
      const now = d.cellsFor(n.id);
      if (now.length > 0) {
        if (focus) moveNext(n.id, now);
        else d.view(now[0]!, false);
      } else if (n.handler !== "") d.open(n.id, glyph(n) === "✦");
    });
    out.push(li);
    if (kids && isOpen) for (const c of n.children) node(c, depth + 1, out);
  };

  const kernelRow = (label: string, out: HTMLElement[]): void => {
    const li = document.createElement("li");
    li.className = "kernel";
    li.textContent = label;
    out.push(li);
  };

  const renderActivity = async (): Promise<void> => {
    const qs = await d.activity().catch(() => [] as Question[]);
    const asks = qs.filter((q) => q.kind !== "main");
    actN.textContent = asks.length > 0 ? String(asks.length) : "";
    actList.replaceChildren(
      ...qs.map((q) => {
        const main = q.kind === "main";
        const li = main
          ? row("main", "★", `${q.from} → ${q.agent}: ${q.head}`, "", "a message to a canvas's main chat — it answers itself; click: open the chat")
          : row("ask", "✉", `${q.from} → ${q.agent}: ${q.head}`, "", "go to the asker — answer there");
        li.addEventListener("click", () => {
          const cells = d.cellsFor(q.agent);
          if (cells.length > 0) moveNext(q.agent, cells);
          else d.open(q.agent, true);
        });
        return li;
      }),
    );
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
      await Promise.all([renderActivity(), renderChats(), renderTree()]);
    },
  };
}
