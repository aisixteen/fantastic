import type { Section } from "../model.ts";

export const canvas: Section = {
  id: "canvas",
  title: "How the canvas works",
  blurb: "Two kernels; cells are views of host agents",
  scenes: [
    {
      id: "kernels",
      title: "The page is a second kernel",
      nodes: [
        { id: "page", label: "page kernel", sub: "ts, in the browser", kind: "kernel", w: 200, wide: { x: 220, y: 110 }, tall: { x: 100, y: 61 }, h: 72 },
        { id: "leg", label: "canvas_ws", sub: "the leg", kind: "agent", w: 150, wide: { x: 500, y: 110 }, tall: { x: 280, y: 61 } },
        { id: "host", label: "host kernel", sub: "fantastic canvas", kind: "kernel", wide: { x: 780, y: 110 }, tall: { x: 460, y: 61 } },
        { id: "canvas", label: "canvas", sub: "geometry · wires", kind: "agent", w: 150, wide: { x: 130, y: 300 }, tall: { x: 110, y: 228 } },
        { id: "bg", label: "background", sub: "one per canvas", kind: "agent", w: 150, wide: { x: 310, y: 300 }, tall: { x: 280, y: 228 } },
        { id: "aicell", label: "AI cell", sub: "ai_core", kind: "ai", w: 140, wide: { x: 690, y: 300 }, tall: { x: 330, y: 365 } },
        { id: "term", label: "terminal", sub: "exec PTY", kind: "agent", w: 140, wide: { x: 870, y: 300 }, tall: { x: 480, y: 365 } },
        { id: "popup", label: "action popup", sub: "double-click", kind: "ui", w: 160, wide: { x: 130, y: 470 }, tall: { x: 110, y: 532 } },
        { id: "you", label: "you", kind: "human", wide: { x: 360, y: 470 }, tall: { x: 330, y: 532 } },
      ],
      edges: [
        { id: "e_ws", from: "page", to: "leg" },
        { id: "e_leg", from: "leg", to: "host" },
        { id: "t_c", from: "page", to: "canvas", tree: true },
        { id: "t_bg", from: "page", to: "bg", tree: true },
        { id: "t_ai", from: "host", to: "aicell", tree: true },
        { id: "t_term", from: "host", to: "term", tree: true },
        { id: "e_dbl", from: "you", to: "popup" },
        { id: "e_pop", from: "popup", to: "canvas" },
      ],
      steps: [
        {
          caption: "fantastic canvas serves a page that runs its own kernel in the browser. The page is a second kernel, not just a view.",
          focus: ["page", "host"],
        },
        {
          caption: "The two kernels talk over the canvas_ws leg. An agent on the page is addressed as <page guid>|<agent>.",
          show: ["leg", "e_ws", "e_leg"],
          flow: [{ edge: "e_ws", label: "hello {guid}" }, { edge: "e_leg", label: "hello" }],
        },
        {
          caption: "Each cell is a view on the page paired with an agent on the host: a terminal, an AI, an html agent.",
          show: ["aicell", "term", "t_ai", "t_term"],
          focus: ["aicell", "term"],
        },
        {
          caption: "The canvas and its background are page agents. The canvas keeps the geometry and the wires; the background is a singleton that can't be deleted.",
          show: ["canvas", "bg", "t_c", "t_bg"],
          focus: ["canvas", "bg"],
        },
        {
          caption: "An AI cell drives the canvas itself: it asks around for its neighbours, then places, moves and wires cells.",
          flow: [
            { edge: "t_ai", label: "send", back: true },
            { edge: "e_leg", label: "<guid>|canvas · around", back: true },
            { edge: "e_ws", label: "around", back: true },
            { edge: "t_c", label: "around" },
          ],
        },
        {
          caption: "Double-click anywhere: the action popup puts a new agent on that spot.",
          show: ["you", "popup", "e_dbl", "e_pop"],
          flow: [{ edge: "e_dbl", label: "double-click" }, { edge: "e_pop", label: "place" }],
        },
      ],
    },
  ],
};
