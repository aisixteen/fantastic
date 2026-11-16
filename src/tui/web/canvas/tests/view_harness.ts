import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

// THE HARNESS FOR AN HTML VIEW'S WHOLE SCRIPT (ONE MODEL, MANY VIEW-MODELS): a view of a base
// agent watches the agents it shows and reads again only on what they publish — no timer
// re-reads anything. The script runs in a vm, on a loose DOM (any element, any property) and a
// fake `fantastic` connector that records every send, watch and timer.
//
// AS THE KERNEL DOES, a watcher receives a COPY OF EVERY CALL made to the agent it watches
// (kernel send.rs fanout; the web leg forwards only the payload): each send here is mirrored to
// the target's watchers, the view's own reads included. A view that acts on a copy loops.
//
// A PRODUCT'S VIEW TESTS IMPORT THIS FILE from the checkout Cargo pins for `fantastic-tui`, at
// `fantastic_tui::VIEW_HARNESS` under that package's manifest directory (`run` takes the html
// itself; `file` reads a file of this repository).

const ROOT = new URL("../../../../../", import.meta.url);
export const file = (rel: string): string => readFileSync(fileURLToPath(new URL(rel, ROOT)), "utf8");
const scriptOf = (html: string): string => {
  const from = html.indexOf("<script>");
  const to = html.indexOf("</script>", from);
  assert.ok(from >= 0 && to > from, "the view has its script");
  return html.slice(from + "<script>".length, to);
};

export type Obj = Record<string, unknown>;
type Reply = (target: string, p: Obj) => unknown;

/** An element that is anything: every property is another one, every call answers one. */
function loose(): unknown {
  const store = new Map<PropertyKey, unknown>();
  const fn = function (): void {};
  return new Proxy(fn, {
    get(_t, k) {
      if (store.has(k)) return store.get(k);
      if (k === Symbol.toPrimitive) return () => "";
      if (k === "then" || k === Symbol.iterator) return undefined;
      const v = loose();
      store.set(k, v);
      return v;
    },
    set(_t, k, v) {
      store.set(k, v);
      return true;
    },
    apply: () => loose(),
    construct: () => loose() as object,
  });
}

export interface Harness {
  sends: { target: string; p: Obj }[];
  watches: Map<string, ((ev: Obj) => void)[]>;
  intervals: { fn: () => void; ms: number }[];
  byId: Map<string, Record<string, unknown>>;
  /** run every timer that is due and let every reply land */
  settle(): Promise<void>;
  /** an event the agent `src` published, to its watchers */
  publish(src: string, ev: Obj): Promise<void>;
  /** the sends made since `mark` */
  since(mark: number): { target: string; p: Obj }[];
  /** a message the host page sends the view (`fantastic.onMessage`) */
  message(m: Obj): Promise<void>;
}

export function run(html: string, fill: Record<string, string>, reply: Reply): Harness {
  let body = scriptOf(html);
  for (const [k, v] of Object.entries(fill)) body = body.replaceAll(`{{${k}}}`, v);
  const sends: Harness["sends"] = [];
  const watches: Harness["watches"] = new Map();
  const intervals: Harness["intervals"] = [];
  let timeouts: (() => void)[] = [];
  const byId = new Map<string, Record<string, unknown>>();
  const ears: ((m: Obj) => void)[] = [];
  const doc = loose() as Record<string, unknown>;
  doc["getElementById"] = (id: string) => {
    if (!byId.has(id)) byId.set(id, loose() as Record<string, unknown>);
    return byId.get(id);
  };
  doc["createElement"] = () => loose();
  const fantastic = {
    send: (target: string, p: Obj) => {
      sends.push({ target, p });
      const copy = JSON.parse(JSON.stringify(p)) as Obj;
      setImmediate(() => (watches.get(target) ?? []).slice().forEach((cb) => cb(copy)));
      return Promise.resolve(JSON.parse(JSON.stringify(reply(target, p) ?? {})));
    },
    watch: (src: string, cb: (ev: Obj) => void) => {
      const l = watches.get(src) ?? [];
      l.push(cb);
      watches.set(src, l);
      return () => {
        const at = l.indexOf(cb);
        if (at >= 0) l.splice(at, 1);
      };
    },
    emit: () => {},
    onMessage: (cb: (m: Obj) => void) => {
      ears.push(cb);
    },
  };
  const ctx = vm.createContext({
    document: doc,
    window: loose(),
    fantastic,
    CSS: { escape: (s: string) => s },
    MutationObserver: class {
      observe(): void {}
    },
    console,
    setInterval: (fn: () => void, ms: number) => {
      intervals.push({ fn, ms });
      return intervals.length;
    },
    setTimeout: (fn: () => void) => {
      timeouts.push(fn);
      return timeouts.length;
    },
    clearTimeout: () => {},
  });
  vm.runInContext(body, ctx);
  const turn = (): Promise<void> => new Promise((r) => setImmediate(r));
  const settle = async (): Promise<void> => {
    for (let i = 0; i < 20; i++) {
      await turn();
      const due = timeouts;
      timeouts = [];
      due.forEach((f) => f());
    }
  };
  return {
    sends,
    watches,
    intervals,
    byId,
    settle,
    async publish(src, ev) {
      const l = watches.get(src) ?? [];
      assert.ok(l.length > 0, `nobody watches ${src}`);
      l.forEach((cb) => cb(ev));
      await settle();
    },
    since: (mark) => sends.slice(mark),
    async message(m) {
      ears.forEach((cb) => cb(JSON.parse(JSON.stringify(m)) as Obj));
      await settle();
    },
  };
}

/** Every interval's tick reads nothing: it only draws again what the view holds. */
export async function clocksReadNothing(h: Harness): Promise<void> {
  await h.settle();
  const mark = h.sends.length;
  h.intervals.forEach((t) => t.fn());
  await h.settle();
  assert.deepEqual(h.since(mark), [], "an interval re-read the agent's state");
}


/** Copies of every call the view made so far (its own reads), fed to it again as its watch
 *  delivers them: none may make it read again. */
export async function ownCopiesReadNothing(h: Harness): Promise<void> {
  await h.settle();
  const mark = h.sends.length;
  for (const c of h.sends.slice(0, mark)) for (const cb of h.watches.get(c.target) ?? []) cb(JSON.parse(JSON.stringify(c.p)) as Obj);
  await h.settle();
  assert.deepEqual(h.since(mark).map((s) => `${s.target} ${String(s.p["type"])}`), [], "a copy of a call made the view read again");
}
