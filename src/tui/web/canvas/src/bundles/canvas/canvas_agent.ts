import { str } from "../../lib.ts";
import type { Handler, Json } from "../../lib.ts";

// THE CANVAS AGENT — the frontend root's own verbs: `around` (geometry, from
// the rendered frames). The background is its own singleton child agent
// (`background_agent.ts`).

export const CANVAS = "canvas.ts";

/** One rendered member, in canvas (world) coordinates. */
export interface Placed {
  id: string;
  kind: string;
  title: string;
  rect: { x: number; y: number; w: number; h: number };
  backend_id?: string;
}

// THE GEOMETRY is the rendered canvas's: `canvas.ts` hands its frames here
// once it is mounted. Before that there is nothing on it to be near.
let placed: (() => Placed[]) | null = null;
export function setGeometry(fn: () => Placed[]): void {
  placed = fn;
}

/** Distance from (x, y) to a rect's nearest edge — 0 inside it. */
export function distance(x: number, y: number, r: Placed["rect"]): number {
  const dx = Math.max(r.x - x, 0, x - (r.x + r.w));
  const dy = Math.max(r.y - y, 0, y - (r.y + r.h));
  return Math.round(Math.hypot(dx, dy));
}

/** `around {x, y, radius}` — the members within `radius` of the point, nearest
 *  first. GEOMETRY ONLY: reflect a member for anything more. */
export function around(members: Placed[], x: number, y: number, radius: number): Array<Placed & { distance: number }> {
  return members
    .map((m) => ({ ...m, distance: distance(x, y, m.rect) }))
    .filter((m) => m.distance <= radius)
    .sort((a, b) => a.distance - b.distance || a.id.localeCompare(b.id));
}

export const CANVAS_README =
  "Canvas compositor (frontend root). Renders the frontend's own member tree; mounts each member inline via its view bundle, or as an iframe for external content. Not a host client. Its BACKGROUND is its own singleton child agent, `background` (set_bg / clear_bg / get_bg there). `around {x:num, y:num, radius:num}` (all required; canvas coordinates, x grows right, y grows down) returns the members within `radius` of the point, nearest first: {members: [{id, kind, title, rect:{x,y,w,h}, distance, backend_id?}]} — geometry only; reflect a member for more.";

export const canvasAgent: Handler = (id, payload, kernel): Json => {
  switch (str(payload, "type")) {
    case "around": {
      const [x, y, radius] = [payload["x"], payload["y"], payload["radius"]];
      const bad = (["x", "y", "radius"] as const).filter((k) => typeof payload[k] !== "number" || !Number.isFinite(payload[k] as number));
      if (bad.length > 0 || (radius as number) <= 0) {
        return {
          error: `canvas: around needs x, y and radius as numbers (radius > 0)${bad.length ? ` — missing or not a number: ${bad.join(", ")}` : ""}`,
          reason: "bad_arg",
          arg: bad[0] ?? "radius",
        };
      }
      if (placed === null) return { error: "canvas: around — the canvas is not rendered (no page has it open)", reason: "not_found" };
      return { members: around(placed(), x as number, y as number, radius as number) } as unknown as Json;
    }
    case "boot":
    case "shutdown":
      return null;
    default:
      return { error: `canvas: unknown verb '${str(payload, "type")}'`, reason: "unhandled" };
  }
};

/** Register the canvas root's bundle. */
export function registerCanvas(kernel: {
  registerBundle(hm: string, h: Handler): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle(CANVAS, canvasAgent);
  kernel.setBundleReadme(CANVAS, CANVAS_README);
}
