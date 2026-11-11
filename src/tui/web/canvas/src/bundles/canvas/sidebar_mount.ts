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
// What the view emits (`select {id}`) is heard on its cell's inbox.
// The open state is not kept: there is no storage in the panel, and no field for it.

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

/** The cell's `can_call` for the view `rec` declares, `"*"` written out for `ids`; null when it is not an html view. */
export function sidebarGrant(rec: Rec | undefined, ids: Iterable<string>): Record<string, string[]> | null {
  if (rec === undefined) return null;
  const v = viewOf(rec);
  return v.kind === "html" && v.bytes !== undefined ? expandReach(cellGrant(SIDEBAR_AGENT, v), ids) : null;
}

/**
 * Mount the column into `mount` (the canvas's page). `onSelect` hears each `select {id}` the view
 * emits. `onPlace` runs after the column's width on the page changes (open or collapsed, beside
 * or over), so what reads the canvas's box follows. Resolves to null when there is no sidebar agent.
 */
export async function mountSidebarColumn(o: {
  kernel: Kernel;
  mount: HTMLElement;
  ids: () => Iterable<string>;
  wide: boolean;
  onSelect: (id: string) => void;
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
  });

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
    setOpen(v: boolean): void {
      open = v;
      paint();
      o.onPlace?.();
    },
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
