import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";

// THE CANVAS-LIST CELL (owner 2026-10-08): nothing floats — a canvas opens as its
// background alone, and this is placed from the double-click like any cell
// (moved, resized, closed; as many as you like).
//
//   canvases_view.ts  every canvas on the host — flat peers; a click opens one,
//                     `+ new canvas` makes a peer (a list, for now)
//
// The agent tree it used to sit beside is the file tree now (`files/files_view.ts`).
// Page content: no backend, no grants. What it reads is the canvas's (set at
// mount by `canvas.ts`).

export const CANVASES_VIEW = "canvases_view.ts";

export interface CanvasEntry {
  id: string;
  main: boolean;
  name: string;
}
export interface CanvasesDeps {
  canvases: CanvasEntry[];
  /** the canvas this page is */
  here: string;
  /** open canvas `id` (its own page kernel: the page reloads onto it) */
  switchTo(id: string): void;
  /** make a peer canvas → its id, or null (said why) */
  newCanvas(name: string): Promise<string | null>;
}

let canvasDeps: CanvasesDeps | null = null;

/** The canvas hands its canvas-list wiring over, at mount. */
export function setCanvasesDeps(canvases: CanvasesDeps): void {
  canvasDeps = canvases;
}

export const canvasesView: ViewBundle = {
  handles: [CANVASES_VIEW],
  grants: () => ({}),
  mount(ctx: ViewContext): ViewHandle {
    if (canvasDeps === null) throw new Error("canvases_view: the canvas is not mounted");
    const d = canvasDeps;
    const el = document.createElement("div");
    el.className = "side";
    el.innerHTML = `<div class="chats-head"><span>canvases</span></div><ul class="canvas-list"></ul><div class="canvas-new">+ new canvas</div>`;
    ctx.mount.appendChild(el);
    const list = el.querySelector(".canvas-list") as HTMLElement;
    list.replaceChildren(
      ...d.canvases.map((c) => {
        const li = document.createElement("li");
        li.className = "canvas" + (c.id === d.here ? " here" : "");
        li.dataset["canvas"] = c.id;
        li.innerHTML = `<span class="dot"></span><span class="nm"></span>`;
        (li.children[0] as HTMLElement).textContent = c.main ? "★" : "◇";
        (li.children[1] as HTMLElement).textContent = c.name;
        li.title = c.id === d.here ? `${c.id} · open now` : `${c.id} · open it`;
        if (c.id !== d.here) li.addEventListener("click", () => d.switchTo(c.id));
        return li;
      }),
    );
    const add = el.querySelector(".canvas-new") as HTMLElement;
    add.addEventListener("click", () => {
      if (add.querySelector("input") !== null) return;
      const input = document.createElement("input");
      input.placeholder = "name, ⏎";
      add.replaceChildren(input);
      input.focus();
      const done = (): void => {
        add.textContent = "+ new canvas";
      };
      input.addEventListener("keydown", (e) => {
        e.stopPropagation();
        if (e.key === "Escape") return done();
        if (e.key !== "Enter" || input.value.trim() === "") return;
        void d.newCanvas(input.value.trim()).then((id) => {
          done();
          if (id !== null) d.switchTo(id);
        });
      });
      input.addEventListener("blur", done);
    });
    return {
      unmount(): void {
        el.remove();
      },
    };
  },
};
