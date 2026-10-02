// THE VIEWER — what the page shows, and how a click in the tree moves it
// (owner, 2026-10-02: "multicanvas … a viewer which switches on which agent you
// click"). Pure: the compositor (`canvas.ts`) owns the DOM.
//
//   · VIEWED is one page agent: a CANVAS (a container: its cells drawn on its
//     own world, its own background) or any other agent (drawn FULL — an html
//     panel as a page, a chat, a terminal, a reflect card).
//   · A canvas may hold CHILD CANVASES (`place {kind: canvas}`): nested in the
//     tree; on its parent each is a cell showing ITS OWN card html, which the
//     child rewrites itself (`set_html` on the child canvas).
//   · A click in the tree VIEWS the agent; ⌘-click (Ctrl-click off macOS —
//     Ctrl-click is the Mac's right click) FOCUSES it instead: its canvas is
//     viewed and the cell centred.
//   · `#view=<id>` is the link: back / forward walk the views.

import { CANVAS } from "./canvas_agent.ts";

export { CANVAS };

/** The slice of a page agent the viewer reads. */
export interface Node {
  id: string;
  parentId: string | null;
  handler: string;
}

export type Lookup = (id: string) => Node | undefined;

/** Whether `id` is a container (a canvas). */
export function isContainer(get: Lookup, id: string): boolean {
  return get(id)?.handler === CANVAS;
}

/** The canvas that shows `id`: itself when it is one, else its nearest
 *  canvas ancestor; null when it sits under none. */
export function containerOf(get: Lookup, id: string): string | null {
  let cur = get(id);
  while (cur !== undefined) {
    if (cur.handler === CANVAS) return cur.id;
    cur = cur.parentId === null ? undefined : get(cur.parentId);
  }
  return null;
}

/** Root → `id`, every id on the way (the breadcrumb). */
export function pathTo(get: Lookup, id: string): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  let cur = get(id);
  while (cur !== undefined && !seen.has(cur.id)) {
    seen.add(cur.id);
    out.unshift(cur.id);
    cur = cur.parentId === null ? undefined : get(cur.parentId);
  }
  return out;
}

/** The view a hash names (`#view=<id>`), or null. */
export function viewFromHash(hash: string): string | null {
  const m = /(?:^#|&)view=([^&]+)/.exec(hash);
  return m ? decodeURIComponent(m[1]!) : null;
}

/** The hash for a view. */
export function hashFor(id: string): string {
  return `#view=${encodeURIComponent(id)}`;
}

/** A FOCUS click (vs a VIEW click): ⌘ on macOS, Ctrl elsewhere. */
export function isFocusClick(e: { metaKey: boolean; ctrlKey: boolean }, mac: boolean): boolean {
  return mac ? e.metaKey : e.ctrlKey;
}

/** Whether this browser runs on macOS. */
export function onMac(): boolean {
  const p = (globalThis.navigator as { platform?: string } | undefined)?.platform ?? "";
  return /Mac|iPhone|iPad/.test(p);
}
