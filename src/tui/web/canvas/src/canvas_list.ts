// THE CANVASES ON THE HOST, read off its agent listing: every `canvas.tools`
// node under `canvas_page`, at ANY depth — a canvas spawned by a canvas is its
// child (a cabinet's, #132), a peer canvas sits under `canvas_page` itself —
// each with its page loader (its `kernel_state` child). Pure: main.ts lists, this
// picks.

type Rec = Record<string, unknown>;

export const CANVAS_PAGE = "canvas_page";
export const CANVAS_NODE = "canvas.tools";
export const LOADER_HM = "kernel_state.tools";

export interface CanvasEntry {
  id: string;
  loader: string;
  root: string;
  main: boolean;
  name: string;
}

/** Whether `id`'s ancestors reach `canvas_page` (a cycle or a missing parent: no). */
function underPage(byId: Map<string, Rec>, id: string): boolean {
  const seen = new Set<string>();
  let p = byId.get(id)?.["parent_id"];
  while (typeof p === "string" && !seen.has(p)) {
    if (p === CANVAS_PAGE) return true;
    seen.add(p);
    p = byId.get(p)?.["parent_id"];
  }
  return false;
}

/** The canvases in `all`, the main one first, then by id. A canvas whose loader
 *  names no root is LOUD (it cannot be opened); one with no loader is skipped. */
export function canvasesOf(all: Rec[]): CanvasEntry[] {
  const byId = new Map<string, Rec>();
  for (const a of all) if (typeof a["id"] === "string") byId.set(a["id"], a);
  const out: CanvasEntry[] = [];
  for (const n of all) {
    if (n["handler_module"] !== CANVAS_NODE || typeof n["id"] !== "string" || !underPage(byId, n["id"])) continue;
    const l = all.find((a) => a["parent_id"] === n["id"] && a["handler_module"] === LOADER_HM);
    if (typeof l?.["id"] !== "string") continue;
    const name = typeof n["display_name"] === "string" && n["display_name"] !== "" ? n["display_name"] : n["id"].replace(/^canvas_/, "");
    if (typeof l["root"] !== "string" || l["root"] === "") throw new Error(`canvas ${n["id"]}: its page loader ${l["id"]} names no root`);
    out.push({ id: n["id"], loader: l["id"], root: l["root"], main: n["main"] === true, name });
  }
  return out.sort((a, b) => Number(b.main) - Number(a.main) || (a.id < b.id ? -1 : 1));
}
