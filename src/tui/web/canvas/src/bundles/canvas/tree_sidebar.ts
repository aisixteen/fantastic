import type { Kernel, Payload } from "../../lib.ts";
import { mountTreePanel } from "../files/tree_panel.ts";
import type { TreePanel } from "../files/tree_panel.ts";
import type { FilesState } from "../files/files_controller.ts";

// THE CANVAS'S OWN SIDEBAR (#148): the file tree, fused into the canvas page. It is
// not an agent and not a cell: the canvas mounts it beside the zoomed world (never
// inside it), fixed to the viewport's left edge, so a zoom or a pan never moves or
// scales it. From a bird's-eye zoom its rows drag onto the canvas like the tree
// cell's do: the same panel (`files/tree_panel.ts`), the same drop (`#canvas`).
// Its open state and its tree's state are kept on the canvas's root view, so a
// reload restores them.

/** The controller's id: the verbs of the tree answer to it (`controllers`), reached through the root view's `tree`. */
export const SIDEBAR_ID = "canvas_tree";

/** The key on the root view's meta that holds the sidebar's state. */
export const SIDEBAR_KEY = "tree_sidebar";

/** The page's classes for the sidebar's place: `tree-beside` on a wide screen (the canvas sits beside it), `tree-open` while it is open. */
export const BESIDE = "tree-beside";
export const OPEN = "tree-open";

/** What is kept of the sidebar: whether it is open, and the tree's folders and selection. */
export interface SidebarState {
  open?: boolean;
  expanded?: string[];
  selected?: string;
}

/** The sidebar's saved state, from a view's meta (`{}` when none, or not one this reads). */
export function sidebarSavedOf(meta: Record<string, unknown> | undefined): SidebarState {
  const s = meta?.[SIDEBAR_KEY];
  if (typeof s !== "object" || s === null || Array.isArray(s)) return {};
  const o = s as Record<string, unknown>;
  return {
    ...(typeof o["open"] === "boolean" ? { open: o["open"] } : {}),
    ...(Array.isArray(o["expanded"]) ? { expanded: o["expanded"].filter((x): x is string => typeof x === "string") } : {}),
    ...(typeof o["selected"] === "string" ? { selected: o["selected"] } : {}),
  };
}

/** Whether the sidebar is open: the saved choice; else open on a wide screen and closed on a narrow one. */
export function sidebarOpenOf(saved: SidebarState, wide: boolean): boolean {
  return saved.open ?? wide;
}

/** Merge `patch` into the sidebar's saved state on `view` (what is saved stays where the patch says nothing). */
export function saveSidebar(kernel: Kernel, view: string, patch: SidebarState): void {
  const cur = sidebarSavedOf(kernel.get(view)?.meta);
  kernel.updateMeta(view, { [SIDEBAR_KEY]: { ...cur, ...patch } } as unknown as Payload);
}

/** The tree's state as the sidebar keeps it: its open folders, and the selection when there is one. */
function treePatch(s: FilesState): SidebarState {
  return { expanded: s.expanded, ...(typeof s.selected === "string" ? { selected: s.selected } : {}) };
}

export interface TreeSidebar {
  /** the sidebar's element: its toggle, and the tree in its body */
  readonly el: HTMLElement;
  /** the tree, mounted in the body */
  readonly tree: TreePanel;
  readonly open: boolean;
  /** open or collapse it; the choice is kept on the view */
  setOpen(open: boolean): void;
  dispose(): void;
}

/**
 * Mount the sidebar into `mount` (the canvas's page, beside its zoomed world) for the
 * canvas whose root view is `view`. `wide` is the screen's width test: the default
 * open state when nothing is saved, and whether the canvas sits beside the sidebar
 * (`tree-beside`: the canvas's margin is the sidebar's width) or under it (a narrow
 * screen's overlay).
 */
export function mountTreeSidebar(o: { kernel: Kernel; view: string; mount: HTMLElement; wide: boolean }): TreeSidebar {
  const { kernel, view } = o;
  const saved = sidebarSavedOf(kernel.get(view)?.meta);
  let open = sidebarOpenOf(saved, o.wide);

  const el = document.createElement("aside");
  el.className = "tree-sidebar";
  const toggle = document.createElement("button");
  toggle.className = "ts-toggle";
  toggle.type = "button";
  const body = document.createElement("div");
  body.className = "ts-body";
  el.appendChild(toggle);
  el.appendChild(body);
  o.mount.appendChild(el);
  if (o.wide) o.mount.classList.add(BESIDE);

  const tree = mountTreePanel({
    kernel,
    selfId: SIDEBAR_ID,
    root: "",
    mount: body,
    persist: (s) => saveSidebar(kernel, view, treePatch(s)),
  });
  void tree.start({ expanded: saved.expanded ?? [], ...(saved.selected !== undefined ? { selected: saved.selected } : {}) });

  const paint = (): void => {
    el.classList.toggle("collapsed", !open);
    o.mount.classList.toggle(OPEN, open);
    toggle.textContent = open ? "‹" : "›";
    toggle.title = open ? "hide the file tree" : "show the file tree";
    toggle.setAttribute("aria-expanded", String(open));
  };
  const setOpen = (v: boolean): void => {
    open = v;
    paint();
    saveSidebar(kernel, view, { open: v });
  };
  toggle.addEventListener("click", () => setOpen(!open));
  paint();

  return {
    el,
    tree,
    get open(): boolean {
      return open;
    },
    setOpen,
    dispose(): void {
      tree.dispose();
      o.mount.classList.remove(BESIDE, OPEN);
      el.remove();
    },
  };
}
