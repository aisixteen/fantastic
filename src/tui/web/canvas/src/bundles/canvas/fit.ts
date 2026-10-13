// FIT TO CONTENT (owner 2026-10-13): the view's `fit {id}` sizes a cell to what
// its panel draws. A panel (`html_view`'s iframe — sandboxed, so the page cannot
// read it) MEASURES ITSELF through its connector and posts `size`: `mw`, the
// width its content wants (max-content), and `h`, its height at the width it has
// now. `fit` sets the width first, then — the panel re-measures at that width
// (a paragraph wraps differently) — the height.

/** What a panel reports about its content, in CSS px. */
export interface ContentSize {
  /** the width the content wants (nothing wrapped that need not be) */
  mw: number;
  /** the content's height at the panel's current width */
  h: number;
  /** wider than the panel at its current width (it cannot wrap to fit) */
  ox?: boolean;
}

const sizes = new Map<string, ContentSize>();
const waiting = new Map<string, Array<(s: ContentSize) => void>>();

/** A panel's report (its connector posts one on load and on every change). */
export function reportSize(cell: string, s: ContentSize): void {
  if (!Number.isFinite(s.mw) || !Number.isFinite(s.h)) return;
  sizes.set(cell, s);
  for (const r of waiting.get(cell)?.splice(0) ?? []) r(s);
}

/** The last report of `cell`, or undefined (not a panel, or not drawn yet). */
export function sizeOf(cell: string): ContentSize | undefined {
  return sizes.get(cell);
}

/** The NEXT report of `cell` — null when none comes within `ms`. */
export function nextSize(cell: string, ms: number): Promise<ContentSize | null> {
  return new Promise((res) => {
    const list = waiting.get(cell) ?? [];
    waiting.set(cell, list);
    const done = (s: ContentSize | null): void => {
      clearTimeout(t);
      const i = list.indexOf(take);
      if (i >= 0) list.splice(i, 1);
      res(s);
    };
    const take = (s: ContentSize): void => done(s);
    const t = setTimeout(() => done(null), ms);
    list.push(take);
  });
}

/** A cell gone: its reports go too. */
export function forgetSize(cell: string): void {
  sizes.delete(cell);
  waiting.delete(cell);
}

/** The smallest a fitted cell gets, and the largest when the call names none. */
export const FIT_MIN = { width: 120, height: 60 };
export const FIT_MAX = { width: 1600, height: 1200 };

/** The cell's size around `content`: plus the frame's own space round its body
 *  (`chrome`), clamped to [FIT_MIN, max]. `overflowed`: the content is bigger
 *  than the clamp lets the cell be (a width over the max may still WRAP to fit —
 *  `fit` asks the panel, measured at the clamped width). */
export function fitSize(
  content: { width: number; height: number },
  chrome: { width: number; height: number },
  max: { width: number; height: number } = FIT_MAX,
): { width: number; height: number; overflowed: boolean } {
  const want = { width: Math.ceil(content.width + chrome.width), height: Math.ceil(content.height + chrome.height) };
  const width = Math.max(FIT_MIN.width, Math.min(max.width, want.width));
  const height = Math.max(FIT_MIN.height, Math.min(max.height, want.height));
  return { width, height, overflowed: want.width > max.width || want.height > max.height };
}

/** The frame's space round its body (its rect minus the body's box), from the
 *  page's DOM; none where there is no DOM (a test). */
export function chromeOf(cell: string): { width: number; height: number } {
  if (typeof document === "undefined") return { width: 0, height: 0 };
  const frame = document.querySelector(`.agent-frame[data-id="${CSS.escape(cell)}"]`) as HTMLElement | null;
  const body = frame?.querySelector(":scope > .frame-body") as HTMLElement | null;
  if (frame === null || body === null || body === undefined) return { width: 0, height: 0 };
  return { width: Math.max(0, frame.offsetWidth - body.clientWidth), height: Math.max(0, frame.offsetHeight - body.clientHeight) };
}

/** The connector's half (inlined into the panel's srcdoc, `connector.ts`): it
 *  measures the document and posts `{op: "size", mw, h}` on load and whenever
 *  the content or the panel's size changes. `mw` is taken with the body at
 *  max-content for one synchronous layout — nothing paints in between. */
export const MEASURE_SRC = `
  const measure = () => {
    const b = document.body;
    if (!b) return;
    const cs = getComputedStyle(b);
    const mx = parseFloat(cs.marginLeft) + parseFloat(cs.marginRight);
    const my = parseFloat(cs.marginTop) + parseFloat(cs.marginBottom);
    const h = Math.max(b.scrollHeight, b.getBoundingClientRect().height) + my;
    const ox = document.documentElement.scrollWidth > document.documentElement.clientWidth + 1;
    const was = b.style.width;
    b.style.width = "max-content";
    const mw = Math.max(b.scrollWidth, b.getBoundingClientRect().width) + mx;
    b.style.width = was;
    // the panel's own width in the key: a resize always reports (\`fit\` waits on it)
    const key = Math.ceil(mw) + "x" + Math.ceil(h) + "@" + innerWidth + (ox ? "!" : "");
    if (key !== last) { last = key; post({ op: "size", mw: Math.ceil(mw), h: Math.ceil(h), ox: ox }); }
  };
  let last = "";
  let queued = false;
  const soon = () => { if (!queued) { queued = true; requestAnimationFrame(() => { queued = false; measure(); }); } };
  addEventListener("load", () => {
    soon();
    new ResizeObserver(soon).observe(document.documentElement);
    if (document.body) new ResizeObserver(soon).observe(document.body);
    // no attributes: measuring sets the body's own style
    new MutationObserver(soon).observe(document.documentElement, { subtree: true, childList: true, characterData: true });
  });
`;
