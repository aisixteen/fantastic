// THE DEMO'S CONTENT MODEL — sections of scenes of steps, all DATA.
//
// A SECTION is a topic ("the kernel and its agents"); it plays one or more
// SCENES. A scene is one schematic diagram — nodes and edges, each with a
// position for a WIDE stage (desktop, landscape) and a TALL one (phones) — and
// a list of STEPS. A step is one beat of the story: a caption, the parts it
// reveals, the parts it puts in focus, and the messages it shows travelling
// along edges. Adding, removing or reordering sections is editing data
// (`sections/index.ts`); nothing in the player knows a section by name.

export type Kind = "human" | "kernel" | "agent" | "ai" | "store" | "ui" | "ext";

export interface Pt {
  x: number;
  y: number;
}

export interface Node {
  id: string;
  label: string;
  /** a second, quieter line under the label */
  sub?: string;
  kind: Kind;
  /** centre on the wide stage (`STAGE.wide`) */
  wide: Pt;
  /** centre on the tall stage (`STAGE.tall`) */
  tall: Pt;
  /** box size; the kind's default when absent */
  w?: number;
  h?: number;
}

export interface Edge {
  id: string;
  from: string;
  to: string;
  /** a structural link (parent → child), drawn quiet and dashed */
  tree?: boolean;
}

/** A message travelling along an edge during a step. */
export interface Flow {
  edge: string;
  label?: string;
  /** travel `to → from` instead of `from → to` (a reply) */
  back?: boolean;
}

export interface Step {
  caption: string;
  /** node and edge ids that appear at this step (and stay) */
  show?: string[];
  /** node ids in focus: the rest of the scene dims while this step plays */
  focus?: string[];
  flow?: Flow[];
}

export interface Scene {
  id: string;
  title: string;
  nodes: Node[];
  edges: Edge[];
  steps: Step[];
}

export interface Section {
  id: string;
  title: string;
  /** one line, for the section list */
  blurb: string;
  scenes: Scene[];
}

/** The two stages, in viewBox units. Every position is authored against one. */
export const STAGE = {
  wide: { w: 1000, h: 560 },
  tall: { w: 560, h: 760 },
} as const;

export type Layout = keyof typeof STAGE;

/** Default box size per kind. */
export function boxOf(n: Node): { w: number; h: number } {
  const base = n.kind === "kernel" ? { w: 200, h: 72 } : n.kind === "human" ? { w: 120, h: 56 } : { w: 168, h: 64 };
  return { w: n.w ?? base.w, h: n.h ?? base.h };
}
