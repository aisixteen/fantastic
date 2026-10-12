// THE DIAGRAM — one scene drawn as SVG on one stage, and a step applied to it.
//
// The scene is drawn once per (scene, layout); a step only toggles classes
// (visible · dim · focus) and runs its flows: a labelled packet that travels
// its edge, again and again while the step plays. Under reduced motion a
// packet rests at the edge's middle instead.

import { along, segment, visibleAt } from "./geometry.ts";
import { boxOf, STAGE } from "./model.ts";
import type { Layout, Scene, Step } from "./model.ts";

const NS = "http://www.w3.org/2000/svg";

function el<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number> = {}): SVGElementTagNameMap[K] {
  const e = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
  return e;
}

export interface Drawn {
  svg: SVGSVGElement;
  scene: Scene;
  layout: Layout;
  /** stop the running flows */
  stop: () => void;
}

/** Draw a scene on a stage. Nothing is visible until a step is applied. */
export function draw(scene: Scene, layout: Layout): Drawn {
  const st = STAGE[layout];
  const svg = el("svg", { viewBox: `0 0 ${st.w} ${st.h}`, class: `diagram ${layout}`, role: "img" });
  const defs = el("defs");
  const marker = el("marker", { id: "arrow", viewBox: "0 0 10 10", refX: 9, refY: 5, markerWidth: 7, markerHeight: 7, orient: "auto-start-reverse" });
  marker.append(el("path", { d: "M0,0 L10,5 L0,10 z", class: "arrowhead" }));
  defs.append(marker);
  svg.append(defs);

  const byId = new Map(scene.nodes.map((n) => [n.id, n]));
  const edges = el("g", { class: "edges" });
  for (const e of scene.edges) {
    const a = byId.get(e.from);
    const b = byId.get(e.to);
    if (!a || !b) continue;
    const [p, q] = segment(a, b, layout);
    const line = el("line", { x1: p.x, y1: p.y, x2: q.x, y2: q.y, class: `edge${e.tree ? " tree" : ""}`, "data-id": e.id });
    if (!e.tree) line.setAttribute("marker-end", "url(#arrow)");
    edges.append(line);
  }
  svg.append(edges);

  const nodes = el("g", { class: "nodes" });
  for (const n of scene.nodes) {
    const { w, h } = boxOf(n);
    const c = n[layout];
    const g = el("g", { class: `node ${n.kind}`, "data-id": n.id, transform: `translate(${c.x - w / 2} ${c.y - h / 2})` });
    g.append(el("rect", { width: w, height: h, rx: n.kind === "human" ? h / 2 : 14 }));
    const label = el("text", { x: w / 2, y: n.sub ? h / 2 - 4 : h / 2 + 6, class: "label" });
    label.textContent = n.label;
    g.append(label);
    if (n.sub) {
      const sub = el("text", { x: w / 2, y: h / 2 + 16, class: "sub" });
      sub.textContent = n.sub;
      g.append(sub);
    }
    nodes.append(g);
  }
  svg.append(nodes);
  svg.append(el("g", { class: "flows" }));
  return { svg, scene, layout, stop: () => {} };
}

const FLOW_MS = 1400;
const GAP_MS = 500;

/** Apply step `i`: what is visible, what is in focus, and its flows. */
export function apply(d: Drawn, i: number, reduced: boolean): void {
  d.stop();
  const step: Step | undefined = d.scene.steps[i];
  const on = visibleAt(d.scene, i);
  const focus = new Set(step?.focus ?? []);
  for (const g of d.svg.querySelectorAll<SVGElement>("[data-id]")) {
    const id = g.dataset["id"] ?? "";
    g.classList.toggle("on", on.has(id));
    g.classList.toggle("focus", focus.has(id));
    g.classList.toggle("dim", focus.size > 0 && g.classList.contains("node") && !focus.has(id));
  }
  d.svg.setAttribute("aria-label", step?.caption ?? d.scene.title);

  const layer = d.svg.querySelector(".flows")!;
  layer.replaceChildren();
  const byId = new Map(d.scene.nodes.map((n) => [n.id, n]));
  const runs = (step?.flow ?? []).flatMap((f) => {
    const e = d.scene.edges.find((x) => x.id === f.edge);
    const a = e && byId.get(e.from);
    const b = e && byId.get(e.to);
    if (!e || !a || !b || !on.has(e.id)) return [];
    let seg = segment(a, b, d.layout);
    if (f.back) seg = [seg[1], seg[0]];
    d.svg.querySelector(`line[data-id="${e.id}"]`)?.classList.add("live");
    const g = el("g", { class: "packet" });
    g.append(el("circle", { r: 9 }));
    if (f.label) {
      const t = el("text", { y: -16 });
      t.textContent = f.label;
      g.append(t);
    }
    layer.append(g);
    return [{ g, seg }];
  });
  // flows of a step leave one after another, then the whole step repeats
  const place = (k: number, t: number) => {
    const r = runs[k];
    if (!r) return;
    const p = along(r.seg, t);
    r.g.setAttribute("transform", `translate(${p.x} ${p.y})`);
  };
  if (reduced || runs.length === 0) {
    runs.forEach((_, k) => place(k, 0.5));
    d.stop = () => {
      for (const l of d.svg.querySelectorAll("line.live")) l.classList.remove("live");
    };
    return;
  }
  const cycle = runs.length * FLOW_MS + GAP_MS;
  const t0 = performance.now();
  let raf = 0;
  const frame = (now: number) => {
    const into = (now - t0) % cycle;
    runs.forEach((r, k) => {
      const local = (into - k * FLOW_MS) / FLOW_MS;
      const shown = local >= 0 && local <= 1;
      r.g.style.opacity = shown ? "1" : "0";
      if (shown) place(k, ease(local));
    });
    raf = requestAnimationFrame(frame);
  };
  raf = requestAnimationFrame(frame);
  d.stop = () => {
    cancelAnimationFrame(raf);
    for (const l of d.svg.querySelectorAll("line.live")) l.classList.remove("live");
  };
}

function ease(t: number): number {
  return t < 0.5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2;
}
