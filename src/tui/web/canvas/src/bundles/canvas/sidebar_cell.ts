import type { Kernel } from "../../lib.ts";
import { hearSidebar } from "../../../../page/view_relay.js";
import type { SidebarEars } from "../../../../page/view_relay.js";

// THE TREE CELL (#608, #698): the host agent `sidebar`'s html view, drawn as a cell on the canvas
// like any other agent's view — moved, sized, hidden and kept like any cell, see-through on the
// canvas. A fresh canvas places one at its left (`SIDEBAR_RECT`). What the view emits is heard on
// its cell's inbox (`hearSidebar`, the base relay module's, the team page's too): `select {id}`,
// `select_file {path}`, `open_file`/`open_dir {path}`, `ready` and `state {state}`
// (which branches and folders are open: kept by the page per canvas, given back as `restore`).

/** The host agent whose view the tree cell shows. */
export const SIDEBAR_AGENT = "sidebar";

/** Where a fresh canvas places the tree cell (world units, the cell's body): below the background's
 *  headline, which is pinned at the screen's top-left (`backgrounds/stock.ts` `headline`). */
export const SIDEBAR_RECT = { x: 16, y: 220, width: 340, height: 640 } as const;

/** Hear what the tree view in cell `cellId` emits; returns the unsubscribe. */
export function hearSidebarCell(kernel: Kernel, cellId: string, ears: SidebarEars): () => void {
  return kernel.onInbox(cellId, (p) => hearSidebar(p as Record<string, unknown>, ears));
}
