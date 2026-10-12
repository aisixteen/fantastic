// PURE GEOMETRY AND VISIBILITY — no DOM, so it is unit-tested.

import { boxOf, STAGE } from "./model.ts";
import type { Layout, Node, Pt, Scene } from "./model.ts";

/** Which stage fits a container: tall when narrow (a phone) or portrait. */
export function pickLayout(width: number, height: number): Layout {
  return width < 700 || height > width * 1.15 ? "tall" : "wide";
}

/** Where the segment from a box's centre toward `to` leaves the box. */
export function exitPoint(c: Pt, w: number, h: number, to: Pt, pad = 6): Pt {
  const dx = to.x - c.x;
  const dy = to.y - c.y;
  if (dx === 0 && dy === 0) return { ...c };
  const hw = w / 2 + pad;
  const hh = h / 2 + pad;
  const t = Math.min(dx === 0 ? Infinity : hw / Math.abs(dx), dy === 0 ? Infinity : hh / Math.abs(dy));
  return { x: c.x + dx * t, y: c.y + dy * t };
}

/** An edge's drawn segment on a stage: border to border. */
export function segment(a: Node, b: Node, layout: Layout): [Pt, Pt] {
  const pa = a[layout];
  const pb = b[layout];
  const ba = boxOf(a);
  const bb = boxOf(b);
  return [exitPoint(pa, ba.w, ba.h, pb), exitPoint(pb, bb.w, bb.h, pa)];
}

/** A point `t` (0..1) of the way along a segment. */
export function along([p, q]: [Pt, Pt], t: number): Pt {
  return { x: p.x + (q.x - p.x) * t, y: p.y + (q.y - p.y) * t };
}

/**
 * The ids visible at step `i`: everything never named in any step's `show`
 * is there from the start; the rest appears at its step and stays. An edge is
 * visible only when both of its ends are.
 */
export function visibleAt(scene: Scene, i: number): Set<string> {
  const revealed = new Set(scene.steps.flatMap((s) => s.show ?? []));
  const on = new Set<string>();
  for (const n of scene.nodes) if (!revealed.has(n.id)) on.add(n.id);
  for (const e of scene.edges) if (!revealed.has(e.id)) on.add(e.id);
  for (const s of scene.steps.slice(0, i + 1)) for (const id of s.show ?? []) on.add(id);
  for (const e of scene.edges) if (!(on.has(e.from) && on.has(e.to))) on.delete(e.id);
  return on;
}

/** Every position of a scene sits inside its stage, box included. */
export function fits(scene: Scene): string[] {
  const bad: string[] = [];
  for (const layout of ["wide", "tall"] as const) {
    const st = STAGE[layout];
    for (const n of scene.nodes) {
      const { w, h } = boxOf(n);
      const p = n[layout];
      if (p.x - w / 2 < 0 || p.x + w / 2 > st.w || p.y - h / 2 < 0 || p.y + h / 2 > st.h) {
        bad.push(`${scene.id}/${n.id} leaves the ${layout} stage`);
      }
    }
  }
  return bad;
}
