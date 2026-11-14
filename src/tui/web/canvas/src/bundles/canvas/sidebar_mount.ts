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
// What the view emits is heard on its cell's inbox (`hearSidebar`): `select {id}` (an agent's cell),
// `select_file {path}` (the previews that follow the selection), `open_file`/`open_dir {path}` (the
// canvas's own `open`), `hide_tree`, and `state {state}`: which branches and folders the view has open.
// The page keeps that state (its own storage, per canvas: the panel has none, and the record has no
// field for it) and gives it back as `restore {state}` when the view says `ready`: it survives a reload.
// The kernel strip's `select_kernel {guid}` is heard and left to the host.

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

/** What the page does with each thing the view says (`hearSidebar`). */
export interface SidebarEars {
  select(id: string): void;
  selectPath(path: string): void;
  open(path: string, dir: boolean): void;
  hide(): void;
  /** the view is up: give it what was kept */
  ready(): void;
  /** the view's open state, to keep */
  keep(state: unknown): void;
}

const text = (v: unknown): string | null => (typeof v === "string" && v !== "" ? v : null);

/** One message the view posted: what it means for the page. Anything else (the view's own echoes, `select_kernel`) is ignored. */
export function hearSidebar(m: Record<string, unknown>, ears: SidebarEars): void {
  const id = text(m["id"]);
  const path = text(m["path"]);
  switch (m["type"]) {
    case "select":
      if (id !== null) ears.select(id);
      break;
    case "select_file":
      if (path !== null) ears.selectPath(path);
      break;
    case "open_file":
    case "open_dir":
      if (path !== null) ears.open(path, m["type"] === "open_dir");
      break;
    case "hide_tree":
      ears.hide();
      break;
    case "ready":
      ears.ready();
      break;
    case "state":
      ears.keep(m["state"]);
      break;
  }
}

/** The page's own storage of the view's open state, one key per canvas. A browser that refuses storage keeps nothing: the view starts closed. */
export interface SidebarStore {
  load(): unknown;
  save(state: unknown): void;
}

export function pageStore(key: string, storage: () => Pick<Storage, "getItem" | "setItem"> = () => localStorage): SidebarStore {
  const name = `fantastic.${key}`;
  return {
    load(): unknown {
      try {
        const raw = storage().getItem(name);
        return raw === null ? null : (JSON.parse(raw) as unknown);
      } catch {
        return null;
      }
    },
    save(state: unknown): void {
      try {
        storage().setItem(name, JSON.stringify(state));
      } catch {
        // no storage: nothing kept
      }
    },
  };
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
  /** a file the view selects (`select_file {path}`) */
  onSelectPath?: (path: string) => void;
  /** a file or folder the view asks to open on the canvas (`open_file`, `open_dir`) */
  onOpen?: (path: string, dir: boolean) => void;
  /** the key the view's open state is kept under (one per canvas); the state is not kept without it */
  storageKey?: string;
  store?: SidebarStore;
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
  const store = o.store ?? (o.storageKey !== undefined ? pageStore(o.storageKey) : undefined);
  const off = kernel.onInbox(SIDEBAR_CELL, (p) =>
    hearSidebar(p as Record<string, unknown>, {
      select: o.onSelect,
      selectPath: (path) => o.onSelectPath?.(path),
      open: (path, dir) => o.onOpen?.(path, dir),
      hide: () => setOpen(false),
      ready: () => {
        const state = store?.load();
        if (state !== null && state !== undefined) kernel.emit(SIDEBAR_CELL, { type: "restore", state } as unknown as Payload);
      },
      keep: (state) => store?.save(state),
    }),
  );

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
  toggle.addEventListener("click", () => setOpen(!open));

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
