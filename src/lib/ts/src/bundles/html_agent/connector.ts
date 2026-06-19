// The iframe-side CONNECTOR — injected into an html_agent's srcdoc as a plain
// <script>. It exposes a kernel-mirroring API (`fantastic.send/emit/watch/
// onMessage`) that talks ONLY to the embedding page via `postMessage`. The page's
// `html_view` relay bridges it to the JS kernel; the JS kernel — the SOLE owner of
// the host link (the kernel bridge) — routes to other FRONTEND agents locally or
// to HOST agents over the bridge, transparently. The iframe has NO host URL, NO
// WebSocket, NO same-origin access: postMessage to its parent is its only channel.
// (This replaces the old `_bridge.js`, which bypassed the JS kernel by dialing the
// host directly — never again.)
//
//   const { job_id } = await fantastic.send("python_runtime_x", { type:"start", code:"print(1+1)" });
//   fantastic.watch("python_runtime_x", (ev) => { if (ev.job_id===job_id) render(ev); }); // live progress
//   fantastic.emit("panel2", { type:"value", value });                     // → another agent
//   const off = fantastic.watch("python_runtime_x", (ev) => …);            // ← agent events
//   fantastic.onMessage((p) => …);                                         // ← messages sent to ME

export const CONNECTOR_SRC = `(() => {
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
  };
})();`;
