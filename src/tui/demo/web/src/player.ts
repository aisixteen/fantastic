// THE PLAYHEAD — where the demo is (section, scene, step) and how it moves.
// Pure: the app owns the clock and the DOM.

import type { Section } from "./model.ts";

export interface Head {
  section: number;
  scene: number;
  step: number;
}

export const START: Head = { section: 0, scene: 0, step: 0 };

/** The next beat: the next step, else the next scene, else the next section;
 *  null at the very end. */
export function next(secs: Section[], h: Head): Head | null {
  const sec = secs[h.section];
  const sc = sec?.scenes[h.scene];
  if (!sec || !sc) return null;
  if (h.step + 1 < sc.steps.length) return { ...h, step: h.step + 1 };
  if (h.scene + 1 < sec.scenes.length) return { ...h, scene: h.scene + 1, step: 0 };
  if (h.section + 1 < secs.length) return { section: h.section + 1, scene: 0, step: 0 };
  return null;
}

/** The previous beat; null at the very start. */
export function prev(secs: Section[], h: Head): Head | null {
  if (h.step > 0) return { ...h, step: h.step - 1 };
  if (h.scene > 0) {
    const sc = secs[h.section]?.scenes[h.scene - 1];
    return sc ? { ...h, scene: h.scene - 1, step: sc.steps.length - 1 } : null;
  }
  if (h.section > 0) {
    const sec = secs[h.section - 1];
    const sc = sec?.scenes[sec.scenes.length - 1];
    return sec && sc ? { section: h.section - 1, scene: sec.scenes.length - 1, step: sc.steps.length - 1 } : null;
  }
  return null;
}

/** `#<section>/<scene>/<step>` (ids, the step 1-based) — a link to one beat. */
export function toHash(secs: Section[], h: Head): string {
  const sec = secs[h.section];
  const sc = sec?.scenes[h.scene];
  return sec && sc ? `#${sec.id}/${sc.id}/${h.step + 1}` : "";
}

/** The beat a hash names; the start when it names nothing known. */
export function fromHash(secs: Section[], hash: string): Head {
  const [secId, scId, stepS] = hash.replace(/^#/, "").split("/");
  const section = secs.findIndex((s) => s.id === secId);
  if (section < 0) return START;
  const scenes = secs[section]!.scenes;
  const scene = Math.max(0, scenes.findIndex((s) => s.id === scId));
  const steps = scenes[scene]?.steps.length ?? 1;
  const step = Math.min(Math.max(0, (Number(stepS) || 1) - 1), steps - 1);
  return { section, scene, step };
}
