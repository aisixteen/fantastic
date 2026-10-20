import { BACKGROUND } from "./background_agent.ts";
import { LAYOUT } from "./layout.ts";
import { SELECTOR } from "./selector_agent.ts";

// A CELL'S CLOSE AND ITS HEAD MENU (#133): ✕ HIDES the cell — the view lets go of
// it, its agent keeps running and stays in the tree. Shift+✕ and the head's
// right-click menu DELETE. Pure: the page wires the intents to its own acts.

/** What a close click or a menu pick does to the cell. */
export type CellIntent = "hide" | "delete";

/** ✕ hides; Shift+✕ deletes. */
export const closeIntent = (shiftKey: boolean): CellIntent => (shiftKey ? "delete" : "hide");

/** The right-click menu on a cell's head, in order (more items come later). */
export const CELL_MENU: ReadonlyArray<{ intent: CellIntent; label: string; title: string }> = [
  { intent: "hide", label: "Hide", title: "hide the cell — its agent keeps running" },
  { intent: "delete", label: "Delete", title: "delete the agent (a locked one is refused)" },
];

/** The page's own agents, which are no cells of a view (a background, a popup,
 *  a layout): a verb never hides or shows them. */
const NOT_CELLS: ReadonlySet<string> = new Set([BACKGROUND, SELECTOR, LAYOUT]);

/** Whether an agent of handler `handler` is a cell of a view. */
export const isCellHandler = (handler: string): boolean => !NOT_CELLS.has(handler);
