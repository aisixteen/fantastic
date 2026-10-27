// WHICH CANVAS THIS PAGE OPENS, from the page's query string: `?canvas=<node id>`
// opens that canvas (an iframe may point at `<page url>?canvas=<id>`); no
// parameter opens the main canvas, else the first. Pure: main.ts reads
// `location.search`, this picks.

import type { CanvasEntry } from "./canvas_list.ts";

export type CanvasPick = CanvasEntry | { missing: string } | { none: true };

export function pickCanvas(canvases: CanvasEntry[], search: string): CanvasPick {
  const asked = new URLSearchParams(search).get("canvas");
  if (asked !== null) return canvases.find((c) => c.id === asked) ?? { missing: asked };
  return canvases.find((c) => c.main) ?? canvases[0] ?? { none: true };
}
