import type { HomeState } from "./home_model.ts";

// THE CANVAS FRAMES — one iframe per open page while its fantastic runs, kept
// across every state that keeps it running at the same url, so switching pages
// never reloads a canvas. `plan` (pure, `tests/home_pages.test.ts`) says what to
// keep, create and drop; `FramePool` applies it to the DOM.
//
// A kept frame is never re-parented or re-`src`-ed: moving an iframe in the DOM
// reloads it.

export interface Plan {
  keep: string[];
  create: Array<[string, string]>;
  drop: string[];
}

/** The canvas url each open page should show now: its row runs, with a canvas. */
function wanted(state: Pick<HomeState, "pages" | "rows">): Map<string, string> {
  const out = new Map<string, string>();
  for (const id of state.pages) {
    const row = state.rows.find((r) => r.id === id);
    if (row?.status === "running" && row.canvas !== null) out.set(id, row.canvas.url);
  }
  return out;
}

/** `held` (page id → the url its frame was made with) against `state`: a running
 *  page at the same url is kept, a running page with no frame is created, and a
 *  frame whose page closed, whose row stopped or whose url changed is dropped (a
 *  changed url is dropped and created again). */
export function plan(held: ReadonlyMap<string, string>, state: Pick<HomeState, "pages" | "rows">): Plan {
  const want = wanted(state);
  const keep: string[] = [];
  const drop: string[] = [];
  for (const [id, url] of held) (want.get(id) === url ? keep : drop).push(id);
  const create: Array<[string, string]> = [...want].filter(([id]) => !keep.includes(id));
  return { keep, create, drop };
}

/** The frames, applied to the DOM. */
export class FramePool {
  private readonly frames = new Map<string, { frame: HTMLIFrameElement; url: string }>();
  private born = 0;

  /** Page id → the url its frame was made with. */
  held(): Map<string, string> {
    return new Map([...this.frames].map(([id, f]) => [id, f.url]));
  }

  /** The frame of page `id`, when it has one. */
  frame(id: string): HTMLIFrameElement | undefined {
    return this.frames.get(id)?.frame;
  }

  /** Apply `p`: drop, create into `slot(id)`, and put a kept frame into its slot
   *  only when it is not there already. */
  apply(p: Plan, slot: (id: string) => HTMLElement): void {
    for (const id of p.drop) {
      this.frames.get(id)?.frame.remove();
      this.frames.delete(id);
    }
    for (const id of p.keep) {
      const f = this.frames.get(id)!.frame;
      const s = slot(id);
      if (f.parentElement !== s) s.append(f);
    }
    for (const [id, url] of p.create) {
      const frame = document.createElement("iframe");
      frame.className = "canvas-frame";
      frame.dataset["born"] = String(++this.born);
      frame.setAttribute("allow", "clipboard-read; clipboard-write; fullscreen");
      frame.src = url;
      slot(id).append(frame);
      this.frames.set(id, { frame, url });
    }
  }

  /** Every frame gone (the view unmounts). */
  clear(): void {
    for (const { frame } of this.frames.values()) frame.remove();
    this.frames.clear();
  }
}
