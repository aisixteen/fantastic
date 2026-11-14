// view_relay.js — THE ONE VIEW RELAY. A view (an html body) runs in a null-origin
// sandboxed `srcdoc` iframe with the `fantastic` connector (`send` / `emit` / `watch` /
// `onMessage` / `openCanvas`) and reaches its page ONLY by postMessage. This module is
// both halves of that: the connector text injected into the iframe (`connectorSrc`) and
// the page-side relay (`mountView`) that answers it. The canvas (`html_view.ts`) and any
// other page of the base web host (the team page) mount views through it: one
// implementation, plain JS, no dependencies.
//
// What a page provides (`transport`): how a send reaches its kernel, how a watch is
// gated and subscribed, where an emit to another agent goes. The relay holds none of
// that: it only speaks the wire (`{__ft, op, rid, target, src, payload}`), keeps the
// bookkeeping (pending watches, unmount, reload), and applies the view's own `calls`
// when the page names them.
//
// Loaded by a page as an ES module: `const m = await import("./view_relay.js")` from the
// host path it is served at (the canvas page's host serves `view_relay.js` beside its
// files), or imported by the canvas bundle.

/** The iframe-side connector source. `extra` is more script a page adds in the iframe
 *  (the canvas adds its size measure); it runs before the view's body. */
export function connectorSrc(extra = "") {
  return `(() => {
  const pending = new Map();
  const watchers = new Map();
  const inboxCbs = new Set();
  let rid = 0;
  const post = (m) => parent.postMessage(Object.assign({ __ft: true }, m), "*");
  addEventListener("message", (e) => {
    const m = e.data;
    if (!m || m.__ft !== true) return;
    if (m.op === "reply") {
      const r = pending.get(m.rid);
      if (r) { pending.delete(m.rid); r(m.data); }
    } else if (m.op === "event") {
      const s = watchers.get(m.src);
      if (s) for (const cb of [...s]) { try { cb(m.payload); } catch (_) {} }
    } else if (m.op === "inbox") {
      for (const cb of [...inboxCbs]) { try { cb(m.payload); } catch (_) {} }
    }
  });
  ${extra}
  globalThis.fantastic = {
    send(target, payload) {
      const id = String(++rid);
      return new Promise((res) => {
        pending.set(id, res);
        post({ op: "send", rid: id, target: target, payload: payload || {} });
      });
    },
    emit(target, payload) { post({ op: "emit", target: target, payload: payload || {} }); },
    watch(src, cb) {
      let s = watchers.get(src);
      if (!s) { s = new Set(); watchers.set(src, s); post({ op: "watch", src: src }); }
      s.add(cb);
      return () => { s.delete(cb); if (s.size === 0) { watchers.delete(src); post({ op: "unwatch", src: src }); } };
    },
    onMessage(cb) { inboxCbs.add(cb); return () => inboxCbs.delete(cb); },
    openCanvas(id) { post({ op: "open_canvas", target: String(id) }); },
  };
})();`;
}

/** Is `verb` on `target` named by a view's `calls` ({target: [verbs]}, `*` as a wildcard
 *  target or verb)? */
export function callAllowed(calls, target, verb) {
  const own = calls[target] || [];
  const any = calls["*"] || [];
  return own.includes(verb) || own.includes("*") || any.includes(verb) || any.includes("*");
}

/**
 * Mount a view's html in `opts.mount`.
 *
 * opts:
 *   mount      host element the iframe is appended to
 *   html       the view's body (a fragment), or a function returning it (or a promise); read at every render
 *   transport  { send(target, payload) -> Promise<reply>,
 *                watch(src, deliver) -> Promise<{off} | {denied: payload}>  (gate and subscribe; `deliver(payload)` per event),
 *                emit?(target, payload) }
 *   onMessage  (m) called for each posted message the relay does not itself answer
 *              (`size`, `open_canvas`, any other op), and for an emit to `cell`
 *   calls      the view's own reach {target: [verbs]}: when given, a send or watch outside it is refused before the transport
 *   cell       the view's own cell id: `{{cell}}` in the html becomes it, and an emit to it is the view
 *              speaking to its page (`onMessage` and the kept-state hooks), not a send
 *   keep       { load() -> state|null, save(state) } kept state: the view emits `{type:"ready"}` and gets
 *              `{type:"restore", state}` back when something is kept; it emits `{type:"state", state}` to keep it
 *   extra      more connector script (see connectorSrc)
 *   trusted    render unsandboxed and same-origin (a first-party view only)
 *   inbox      subscribe(deliver) -> off: messages for the view (its cell's inbox); `reload_html` re-renders instead
 *   win, doc   window and document, for a host that is not the page (tests)
 *
 * Returns { iframe, reload(), inbox(payload), unmount() }.
 */
export function mountView(opts) {
  const win = opts.win || globalThis.window;
  const doc = opts.doc || globalThis.document;
  const transport = opts.transport;
  const iframe = doc.createElement("iframe");
  if (!opts.trusted) iframe.setAttribute("sandbox", "allow-scripts");
  iframe.setAttribute("allow", "autoplay");
  const connector = connectorSrc(opts.extra || "");
  const render = () => {
    const src = typeof opts.html === "function" ? opts.html() : opts.html;
    return Promise.resolve(src).then((b) => {
      const body = opts.cell === undefined ? String(b) : String(b).split("{{cell}}").join(opts.cell);
      iframe.srcdoc = `<!doctype html><html><head><meta charset="utf-8"><script>${connector}</script></head><body>${body}</body></html>`;
    });
  };
  void render();
  opts.mount.appendChild(iframe);

  const toIframe = (m) => {
    if (iframe.contentWindow) iframe.contentWindow.postMessage(Object.assign({ __ft: true }, m), "*");
  };
  const inbox = (payload) => toIframe({ op: "inbox", payload });
  const watchOffs = new Map();
  const denial = (target, verb) => ({ error: `the view may not call ${target} ${verb}`, reason: "denied" });
  const reach = (target, verb) => opts.calls === undefined || callAllowed(opts.calls, target, verb);

  const offInbox = opts.inbox
    ? opts.inbox((p) => {
        if (p && p.type === "reload_html") void render();
        else inbox(p);
      })
    : () => {};

  const onMsg = (e) => {
    if (e.source !== iframe.contentWindow) return; // scope strictly to OUR iframe
    const m = e.data;
    if (!m || m.__ft !== true) return;
    switch (m.op) {
      case "send": {
        const target = m.target ?? "";
        const payload = m.payload ?? {};
        if (!reach(target, String(payload.type))) {
          toIframe({ op: "reply", rid: m.rid, data: denial(target, String(payload.type)) });
          break;
        }
        void Promise.resolve(transport.send(target, payload)).then(
          (data) => toIframe({ op: "reply", rid: m.rid, data }),
          (x) => toIframe({ op: "reply", rid: m.rid, data: { error: String((x && x.message) || x) } }),
        );
        break;
      }
      case "emit": {
        const payload = m.payload ?? {};
        if (opts.cell !== undefined && m.target === opts.cell) {
          if (opts.keep && payload.type === "ready") {
            const s = opts.keep.load();
            if (s !== null && s !== undefined) inbox({ type: "restore", state: s });
          } else if (opts.keep && payload.type === "state") opts.keep.save(payload.state);
          if (opts.onMessage) opts.onMessage(payload, m);
        } else if (transport.emit) void transport.emit(m.target ?? "", payload);
        else if (opts.onMessage) opts.onMessage(m);
        break;
      }
      case "watch": {
        const src = m.src ?? "";
        if (watchOffs.has(src)) break;
        if (!reach(src, "watch")) {
          toIframe({ op: "event", src, payload: denial(src, "watch") });
          break;
        }
        // a placeholder while the gate answers: a second watch is not doubled
        const pending = () => {};
        watchOffs.set(src, pending);
        void Promise.resolve(transport.watch(src, (payload) => toIframe({ op: "event", src, payload }))).then((got) => {
          // unwatched or unmounted while the gate answered: undo what was subscribed, attach nothing
          if (watchOffs.get(src) !== pending) {
            if (got && got.off) got.off();
            return;
          }
          if (!got || got.denied !== undefined) {
            watchOffs.delete(src);
            toIframe({ op: "event", src, payload: got ? got.denied : { error: "watch refused" } });
            return;
          }
          watchOffs.set(src, got.off);
        });
        break;
      }
      case "unwatch": {
        const src = m.src ?? "";
        const off = watchOffs.get(src);
        if (off) off();
        watchOffs.delete(src);
        break;
      }
      default:
        if (opts.onMessage) opts.onMessage(m);
    }
  };
  win.addEventListener("message", onMsg);

  return {
    iframe,
    reload: () => void render(),
    inbox,
    unmount() {
      win.removeEventListener("message", onMsg);
      offInbox();
      for (const off of watchOffs.values()) off();
      watchOffs.clear();
      iframe.remove();
    },
  };
}

// ---- what a page does with the base SIDEBAR view (the canvas's column and the team's left cell) ----

const text = (v) => (typeof v === "string" && v !== "" ? v : null);

/** One message the sidebar view posted: what it means for the page. `ears` has any of
 *  `select(id)`, `selectPath(path)`, `open(path, dir)`, `hide()`, `ready()`, `keep(state)`;
 *  one a page does not need is left out. Anything else (the view's own echoes,
 *  `select_kernel`) is ignored. */
export function hearSidebar(m, ears) {
  const id = text(m.id);
  const path = text(m.path);
  switch (m.type) {
    case "select":
      if (id !== null && ears.select) ears.select(id);
      break;
    case "select_file":
      if (path !== null && ears.selectPath) ears.selectPath(path);
      break;
    case "open_file":
    case "open_dir":
      if (path !== null && ears.open) ears.open(path, m.type === "open_dir");
      break;
    case "hide_tree":
      if (ears.hide) ears.hide();
      break;
    case "ready":
      if (ears.ready) ears.ready();
      break;
    case "state":
      if (ears.keep) ears.keep(m.state);
      break;
  }
}

/** The page's own storage of the view's open state, one key per page (`fantastic.<key>`).
 *  A browser that refuses storage keeps nothing: the view starts closed. It is the `keep`
 *  of `mountView` as it stands. */
export function pageStore(key, storage = () => localStorage) {
  const name = `fantastic.${key}`;
  return {
    load() {
      try {
        const raw = storage().getItem(name);
        return raw === null ? null : JSON.parse(raw);
      } catch {
        return null;
      }
    },
    save(state) {
      try {
        storage().setItem(name, JSON.stringify(state));
      } catch {
        // no storage: nothing kept
      }
    },
  };
}
