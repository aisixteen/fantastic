import { Agent } from "../../lib.ts";
import type { Kernel, Payload } from "../../lib.ts";
import { mountHostPanel } from "../agent_view/agent_view.ts";
import { AGENT_VIEW } from "../agent_view/agent_view.ts";
import { cellGrant, expandReach, viewOf, viewRecordFresh } from "./host_views.ts";
import type { Rec } from "./host_views.ts";

// THE CANVAS'S LEFT COLUMN (#608): the host agent `sidebar`'s html view, drawn by its
// `reflect` in a panel fixed to the viewport's left edge, beside the zoomed world (never in
// it): a zoom or a pan does not move it. The view is the agent's; this is only the place it
// sits and the grant of the cell that shows it (`cellGrant`, with the view's `"*"` reach
// written out per agent). No `sidebar` agent (a folder seeded before it): no column.
// What the view emits is heard on its cell's inbox: `select {id}` (an agent), `open_file {path}` and
// `open_dir {path}` (a row clicked), `open_on_canvas {items}` (a row dragged out of the column: no
// drop point, the canvas places it as its `open` does), `hide` (the "Hide the tree" button),
// `ready` and `state {state}` (what the view keeps across a reload: the folds, the open folders,
// the files switch). The view has no storage of its own (a null-origin frame): this page keeps the
// state it is told, in the browser, and gives it back on `ready` as `restore {state}`.

/** The host agent whose view this is. */
export const SIDEBAR_AGENT = "sidebar";
/** The page cell that shows it (a page agent under the root: not a cell of any view). */
export const SIDEBAR_CELL = "view_sidebar";

/** The page's classes for the column's place: `sidebar-beside` on a wide screen (the canvas sits beside it), `sidebar-open` while it is open. */
export const BESIDE = "sidebar-beside";
export const OPEN = "sidebar-open";

export interface SidebarColumn {
  readonly el: HTMLElement;
  readonly open: boolean;
  /** open or collapse it */
  setOpen(open: boolean): void;
  /** the screen is wide (`true`: beside the canvas) or narrow (`false`: over it) */
  setWide(wide: boolean): void;
  /** the sidebar agent's `view` as reflected now, and the host agents that exist now: the cell's grant follows */
  follow(rec: Rec | undefined, ids: Iterable<string>): void;
  dispose(): void;
}

/** What the view asks the canvas to open: a project path, or an agent by id. */
export type OpenItem = { path: string } | { agent: string };

/** The items an `open_file` / `open_dir` / `open_on_canvas` message names; null when it is none of them or names nothing usable. */
export function openItems(m: Record<string, unknown>): OpenItem[] | null {
  const t = m["type"];
  if (t === "open_file" || t === "open_dir") return typeof m["path"] === "string" && m["path"] !== "" ? [{ path: m["path"] }] : null;
  if (t !== "open_on_canvas" || !Array.isArray(m["items"])) return null;
  const out: OpenItem[] = [];
  for (const it of m["items"] as unknown[]) {
    const o = (it ?? {}) as Record<string, unknown>;
    if ((o["kind"] === "file" || o["kind"] === "dir") && typeof o["path"] === "string" && o["path"] !== "") out.push({ path: o["path"] });
    else if (o["kind"] === "agent" && typeof o["id"] === "string" && o["id"] !== "") out.push({ agent: o["id"] });
  }
  return out.length > 0 ? out : null;
}

/** Where the view's kept state lives in the browser (a per-viewer convenience, never needed). */
export const STATE_KEY = "fantastic.sidebar.state";

const readState = (): unknown => {
  try {
    const s = localStorage.getItem(STATE_KEY);
    return s === null ? null : JSON.parse(s);
  } catch {
    return null;
  }
};
const writeState = (s: unknown): void => {
  try {
    localStorage.setItem(STATE_KEY, JSON.stringify(s));
  } catch {
    // no storage: the state is simply not kept
  }
};

/** The cell's `can_call` for the view `rec` declares, `"*"` written out for `ids`; null when it is not an html view. */
export function sidebarGrant(rec: Rec | undefined, ids: Iterable<string>): Record<string, string[]> | null {
  if (rec === undefined) return null;
  const v = viewOf(rec);
  return v.kind === "html" && v.bytes !== undefined ? expandReach(cellGrant(SIDEBAR_AGENT, v), ids) : null;
}

/**
 * Mount the column into `mount` (the canvas's page). `onSelect` hears each `select {id}` the view
 * emits, `onOpen` each open request. `onPlace` runs after the column's width on the page changes (open or collapsed, beside
 * or over), so what reads the canvas's box follows. Resolves to null when there is no sidebar agent.
 */
export async function mountSidebarColumn(o: {
  kernel: Kernel;
  mount: HTMLElement;
  ids: () => Iterable<string>;
  wide: boolean;
  onSelect: (id: string) => void;
  /** a row to open on the canvas (`open {path}` or `open {agent}`, as its drop does) */
  onOpen?: (items: OpenItem[]) => void;
  onPlace?: () => void;
}): Promise<SidebarColumn | null> {
  const { kernel } = o;
  const grant = sidebarGrant(await viewRecordFresh(SIDEBAR_AGENT), o.ids());
  if (grant === null) return null;
  kernel.register(
    new Agent({
      id: SIDEBAR_CELL,
      parentId: kernel.rootId,
      handlerModule: AGENT_VIEW,
      meta: { backend_id: SIDEBAR_AGENT, owns_backend: false, can_call: grant } as unknown as Payload,
    }),
  );
  let open = o.wide;
  let wide = o.wide;

  const el = document.createElement("aside");
  el.className = "sidebar-col";
  const toggle = document.createElement("button");
  toggle.className = "sb-toggle";
  toggle.type = "button";
  const body = document.createElement("div");
  body.className = "sb-body";
  el.append(toggle, body);
  o.mount.appendChild(el);

  const handle = mountHostPanel({ kernel, mount: body, selfId: SIDEBAR_CELL, backend: SIDEBAR_AGENT, close: () => {} }, SIDEBAR_AGENT);
  const off = kernel.onInbox(SIDEBAR_CELL, (p) => {
    const m = p as Record<string, unknown>;
    if (m["type"] === "select" && typeof m["id"] === "string" && m["id"] !== "") o.onSelect(m["id"]);
    // `select_kernel {guid}`: the host decides; the canvas does nothing yet
    const items = openItems(m);
    if (items !== null) o.onOpen?.(items);
    if (m["type"] === "hide") setOpen(false);
    if (m["type"] === "state") writeState(m["state"]);
    if (m["type"] === "ready") {
      const kept = readState();
      if (kept !== null) kernel.emit(SIDEBAR_CELL, { type: "restore", state: kept } as unknown as Payload);
    }
  });

  const setOpen = (v: boolean): void => {
    open = v;
    paint();
    o.onPlace?.();
  };
  const place = (): void => {
    o.mount.classList.toggle(BESIDE, wide);
  };
  const paint = (): void => {
    el.classList.toggle("collapsed", !open);
    o.mount.classList.toggle(OPEN, open);
    toggle.textContent = open ? "‹" : "›";
    toggle.title = open ? "hide the sidebar" : "show the sidebar";
    toggle.setAttribute("aria-expanded", String(open));
  };
  place();
  paint();
  toggle.addEventListener("click", () => {
    open = !open;
    paint();
    o.onPlace?.();
  });

  return {
    el,
    get open(): boolean {
      return open;
    },
    setOpen,
    setWide(v: boolean): void {
      wide = v;
      place();
      o.onPlace?.();
    },
    follow(rec: Rec | undefined, ids: Iterable<string>): void {
      const want = sidebarGrant(rec, ids);
      if (want === null) return;
      const have = kernel.get(SIDEBAR_CELL)?.meta["can_call"];
      if (JSON.stringify(have) !== JSON.stringify(want)) kernel.updateMeta(SIDEBAR_CELL, { can_call: want } as unknown as Payload);
    },
    dispose(): void {
      off();
      handle.unmount();
      kernel.remove(SIDEBAR_CELL);
      o.mount.classList.remove(BESIDE, OPEN);
      el.remove();
    },
  };
}
