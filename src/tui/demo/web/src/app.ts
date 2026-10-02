// THE DEMO SHELL — the section list, the stage, the caption and the controls,
// wired to the playhead. One page for a desktop and a phone: the stage picks
// the wide or tall diagram from its own size, and swipes, arrow keys and
// buttons all move the same playhead. A beat is a link (`#section/scene/step`).

import { apply, draw } from "./diagram.ts";
import type { Drawn } from "./diagram.ts";
import { pickLayout } from "./geometry.ts";
import type { Layout, Section } from "./model.ts";
import { fromHash, next, prev, toHash } from "./player.ts";
import type { Head } from "./player.ts";

/** How long a beat stays on screen while playing. */
const BEAT_MS = 4200;

function h<K extends keyof HTMLElementTagNameMap>(tag: K, cls = "", text = ""): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text) e.textContent = text;
  return e;
}

export function mount(root: HTMLElement, sections: Section[]): void {
  const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
  root.replaceChildren();
  root.classList.add("demo");

  const nav = h("nav", "sections");
  nav.setAttribute("aria-label", "Sections");
  const list = h("ol");
  const items = sections.map((s, i) => {
    const li = h("li");
    const b = h("button", "section-btn");
    b.append(h("span", "n", String(i + 1).padStart(2, "0")), h("span", "t", s.title), h("span", "b", s.blurb));
    b.addEventListener("click", () => go({ section: i, scene: 0, step: 0 }, true));
    li.append(b);
    list.append(li);
    return b;
  });
  nav.append(h("div", "brand", "Aisixteen Fantastic"), h("div", "tag", "how it works"), list);

  const main = h("main", "stage-col");
  const head = h("header", "scene-head");
  const kicker = h("div", "kicker");
  const title = h("h1", "scene-title");
  head.append(kicker, title);
  const stage = h("div", "stage");
  const caption = h("p", "caption");
  caption.setAttribute("aria-live", "polite");
  const controls = h("div", "controls");
  const back = h("button", "ctl", "‹");
  back.setAttribute("aria-label", "Previous");
  const play = h("button", "ctl play");
  const fwd = h("button", "ctl", "›");
  fwd.setAttribute("aria-label", "Next");
  const dots = h("div", "dots");
  controls.append(back, play, fwd, dots);
  main.append(head, stage, caption, controls);
  root.append(nav, main);

  let at: Head = fromHash(sections, location.hash);
  let drawn: Drawn | null = null;
  let drawnKey = "";
  let playing = !reduced;
  let timer = 0;

  const layoutNow = (): Layout => pickLayout(stage.clientWidth, Math.max(stage.clientHeight, window.innerHeight * 0.55));

  function render(): void {
    const sec = sections[at.section];
    const sc = sec?.scenes[at.scene];
    if (!sec || !sc) return;
    const layout = layoutNow();
    const key = `${sec.id}/${sc.id}/${layout}`;
    if (key !== drawnKey) {
      drawn?.stop();
      drawn = draw(sc, layout);
      drawnKey = key;
      stage.replaceChildren(drawn.svg);
    }
    apply(drawn!, at.step, reduced);
    items.forEach((b, i) => b.classList.toggle("current", i === at.section));
    kicker.textContent = `${String(at.section + 1).padStart(2, "0")} · ${sec.title}`;
    title.textContent = sc.title;
    caption.textContent = sc.steps[at.step]?.caption ?? "";
    dots.replaceChildren(
      ...sc.steps.map((_, i) => {
        const d = h("button", `dot${i === at.step ? " on" : ""}`);
        d.setAttribute("aria-label", `Step ${i + 1}`);
        d.addEventListener("click", () => go({ ...at, step: i }, true));
        return d;
      }),
    );
    back.disabled = prev(sections, at) === null;
    fwd.disabled = next(sections, at) === null;
    play.textContent = playing ? "❚❚" : "▶";
    play.setAttribute("aria-label", playing ? "Pause" : "Play");
    const hash = toHash(sections, at);
    if (location.hash !== hash) history.replaceState(null, "", hash);
  }

  function schedule(): void {
    clearTimeout(timer);
    if (!playing) return;
    timer = window.setTimeout(() => {
      const n = next(sections, at);
      if (n) go(n, false);
      else {
        playing = false;
        render();
      }
    }, BEAT_MS);
  }

  /** Move the playhead. A human's move pauses autoplay. */
  function go(to: Head, byHuman: boolean): void {
    at = to;
    if (byHuman) playing = false;
    render();
    schedule();
  }

  back.addEventListener("click", () => {
    const p = prev(sections, at);
    if (p) go(p, true);
  });
  fwd.addEventListener("click", () => {
    const n = next(sections, at);
    if (n) go(n, true);
  });
  play.addEventListener("click", () => {
    playing = !playing;
    if (playing && next(sections, at) === null) at = { section: 0, scene: 0, step: 0 };
    render();
    schedule();
  });
  window.addEventListener("keydown", (e) => {
    if (e.key === "ArrowRight") fwd.click();
    else if (e.key === "ArrowLeft") back.click();
    else if (e.key === " ") {
      e.preventDefault();
      play.click();
    }
  });
  // a horizontal swipe on the stage is a step
  let sx = 0;
  let sy = 0;
  stage.addEventListener("touchstart", (e) => {
    const t = e.touches[0];
    if (t) [sx, sy] = [t.clientX, t.clientY];
  }, { passive: true });
  stage.addEventListener("touchend", (e) => {
    const t = e.changedTouches[0];
    if (!t) return;
    const dx = t.clientX - sx;
    if (Math.abs(dx) > 48 && Math.abs(dx) > Math.abs(t.clientY - sy) * 1.5) (dx < 0 ? fwd : back).click();
  });
  window.addEventListener("hashchange", () => go(fromHash(sections, location.hash), true));
  new ResizeObserver(() => render()).observe(stage);

  render();
  schedule();
}
