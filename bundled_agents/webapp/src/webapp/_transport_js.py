"""Inline browser transport — IIFE served at /_fantastic/transport.js.

Exposes window.fantastic_transport() returning:
  - call/emit/watch/unwatch/on/onAny/dispatcher: kernel-routed (text or binary WS frames)
  - bus.send/bus.broadcast/bus.on/bus.onAny: BroadcastChannel("fantastic"),
    browser-only, bypasses the kernel entirely (structured-clone payloads —
    bytes, objects, strings, all native).

Two on-wire formats for kernel traffic, auto-selected by payload content:
  - text frames: JSON (no bytes anywhere in payload)
  - binary frames: [4-byte BE length | JSON header | raw bytes body],
    header has bytes-field nulled and `_binary_path` naming the field.
"""

TRANSPORT_JS = r"""
(function () {
  function parseAgentId() {
    var segs = location.pathname.split('/').filter(Boolean);
    if (segs.length === 0) return '';
    for (var i = segs.length - 1; i >= 0; i--) {
      if (segs[i] && !/\.[a-z]+$/i.test(segs[i])) return segs[i];
    }
    return '';
  }

  // ─── universal binary path helpers ───
  function findBinaryPath(obj, prefix) {
    prefix = prefix || '';
    if (obj instanceof ArrayBuffer || ArrayBuffer.isView(obj)) {
      return prefix;
    }
    if (obj && typeof obj === 'object' && !Array.isArray(obj)) {
      for (var k in obj) {
        if (!Object.prototype.hasOwnProperty.call(obj, k)) continue;
        var p = prefix ? prefix + '.' + k : k;
        var r = findBinaryPath(obj[k], p);
        if (r !== null) return r;
      }
    } else if (Array.isArray(obj)) {
      for (var i = 0; i < obj.length; i++) {
        var p2 = prefix ? prefix + '.' + i : '' + i;
        var r2 = findBinaryPath(obj[i], p2);
        if (r2 !== null) return r2;
      }
    }
    return null;
  }
  function getPath(obj, path) {
    var parts = path.split('.');
    var cur = obj;
    for (var i = 0; i < parts.length; i++) {
      cur = Array.isArray(cur) ? cur[parseInt(parts[i], 10)] : cur[parts[i]];
    }
    return cur;
  }
  function setPath(obj, path, value) {
    var parts = path.split('.');
    var cur = obj;
    for (var i = 0; i < parts.length - 1; i++) {
      cur = Array.isArray(cur) ? cur[parseInt(parts[i], 10)] : cur[parts[i]];
    }
    var last = parts[parts.length - 1];
    if (Array.isArray(cur)) cur[parseInt(last, 10)] = value;
    else cur[last] = value;
  }
  function deepClone(obj) {
    if (obj === null || typeof obj !== 'object') return obj;
    if (Array.isArray(obj)) return obj.map(deepClone);
    var out = {};
    for (var k in obj) if (Object.prototype.hasOwnProperty.call(obj, k)) out[k] = deepClone(obj[k]);
    return out;
  }
  function asArrayBuffer(v) {
    if (v instanceof ArrayBuffer) return v;
    if (ArrayBuffer.isView(v)) return v.buffer.slice(v.byteOffset, v.byteOffset + v.byteLength);
    return null;
  }
  function encodeFrame(envelope) {
    var path = findBinaryPath(envelope);
    if (path === null) {
      return { data: JSON.stringify(envelope), binary: false };
    }
    var body = asArrayBuffer(getPath(envelope, path));
    var head = deepClone(envelope);
    setPath(head, path, null);
    head._binary_path = path;
    var headStr = JSON.stringify(head);
    var headBytes = new TextEncoder().encode(headStr);
    var frame = new ArrayBuffer(4 + headBytes.length + body.byteLength);
    var view = new DataView(frame);
    view.setUint32(0, headBytes.length, false);
    new Uint8Array(frame, 4, headBytes.length).set(headBytes);
    new Uint8Array(frame, 4 + headBytes.length).set(new Uint8Array(body));
    return { data: frame, binary: true };
  }
  function decodeFrame(data) {
    if (typeof data === 'string') return JSON.parse(data);
    var view = new DataView(data);
    var headLen = view.getUint32(0, false);
    var headStr = new TextDecoder().decode(new Uint8Array(data, 4, headLen));
    var head = JSON.parse(headStr);
    var body = new Uint8Array(data, 4 + headLen);
    var path = head._binary_path;
    delete head._binary_path;
    if (path) setPath(head, path, body);
    return head;
  }

  window.fantastic_transport = function () {
    var agentId = parseAgentId();
    var wsUrl = (location.protocol === 'https:' ? 'wss://' : 'ws://') +
                location.host + '/' + agentId + '/ws';

    // Auto-reconnect with exponential backoff. The previous connect-once
    // design left tabs stuck on stale state across server restarts: the
    // WS died silently, `t.call` would either hang or throw inside a
    // Promise executor (uncaught), and the UI never noticed. New design:
    //   - ws.onclose schedules a reconnect (1s → 2s → 4s … cap 16s)
    //   - on reopen, replay standing watches + state_subscribe so
    //     observers resume seamlessly
    //   - pending calls reject with Error('disconnected') so callers
    //     can show an error / reset busy state instead of waiting
    //     forever
    //   - lifecycle handlers ('connected'|'disconnected') let the UI
    //     react to the transition (e.g. clear a stuck inflight bubble)
    var ws = null;
    var connected = false;
    var pending = {};
    var listeners = {};
    var anyListeners = [];
    var stateHandlers = [];     // subscribers to the kernel state stream
    var watching = {};          // src -> true; replayed on reconnect
    var nextId = 1;
    var lifecycleHandlers = []; // ('connected'|'disconnected') callbacks

    var reconnectDelay = 1000;
    var MAX_RECONNECT_DELAY = 16000;
    var readyResolve = null;
    var ready = new Promise(function (res) { readyResolve = res; });

    function fireLifecycle(state) {
      lifecycleHandlers.forEach(function (h) { try { h(state); } catch (e) {} });
    }

    function rejectAllPending() {
      var snapshot = pending;
      pending = {};
      for (var id in snapshot) {
        try { snapshot[id].reject(new Error('disconnected')); } catch (e) {}
      }
    }

    function connect() {
      ws = new WebSocket(wsUrl);
      ws.binaryType = 'arraybuffer';
      ws.onopen = function () {
        connected = true;
        reconnectDelay = 1000;
        // Replay any standing subscriptions so the new connection
        // delivers the same event flow as the dead one.
        for (var src in watching) sendFrame({ type: 'watch', src: src });
        if (stateHandlers.length > 0) sendFrame({ type: 'state_subscribe' });
        if (readyResolve) {
          var r = readyResolve; readyResolve = null; r();
        }
        fireLifecycle('connected');
      };
      ws.onmessage = function (ev) {
        var msg = decodeFrame(ev.data);
        if (msg.type === 'reply') {
          var p = pending[msg.id];
          if (p) {
            delete pending[msg.id];
            if (msg.error) p.reject(new Error(msg.error));
            else p.resolve(msg.data);
          }
        } else if (msg.type === 'event') {
          var t = msg.payload && msg.payload.type;
          if (t && listeners[t]) listeners[t].forEach(function (h) { try { h(msg.payload); } catch (e) {} });
          anyListeners.forEach(function (h) { try { h(t, msg.payload); } catch (e) {} });
        } else if (msg.type === 'state_snapshot' || msg.type === 'state_event') {
          // Telemetry stream from kernel — direct top-level frame, NOT
          // wrapped in {type:'event', payload}. Dispatch to every
          // subscribeState() handler.
          stateHandlers.forEach(function (h) { try { h(msg); } catch (e) {} });
        }
      };
      ws.onerror = function () { /* close will follow */ };
      ws.onclose = function () {
        connected = false;
        rejectAllPending();
        fireLifecycle('disconnected');
        setTimeout(connect, reconnectDelay);
        reconnectDelay = Math.min(reconnectDelay * 2, MAX_RECONNECT_DELAY);
      };
    }

    function sendFrame(envelope) {
      if (!connected) return false;
      try {
        var f = encodeFrame(envelope);
        ws.send(f.data);
        return true;
      } catch (e) {
        return false;
      }
    }

    function call(target, payload) {
      if (!connected) return Promise.reject(new Error('disconnected'));
      return new Promise(function (resolve, reject) {
        var id = String(nextId++);
        pending[id] = { resolve: resolve, reject: reject };
        if (!sendFrame({ type: 'call', target: target, payload: payload, id: id })) {
          delete pending[id];
          reject(new Error('disconnected'));
        }
      });
    }
    function emit(target, payload) {
      // Fire-and-forget; drop on disconnect (no buffering of
      // arbitrary emits — replay would re-deliver stale events).
      sendFrame({ type: 'emit', target: target, payload: payload });
    }
    function watch(src) {
      // Track + send. The set is replayed on reconnect so callers
      // don't need to re-subscribe.
      watching[src] = true;
      sendFrame({ type: 'watch', src: src });
    }
    function unwatch(src) {
      delete watching[src];
      sendFrame({ type: 'unwatch', src: src });
    }
    function on(event_type, handler) {
      (listeners[event_type] = listeners[event_type] || []).push(handler);
      return function off() {
        listeners[event_type] = (listeners[event_type] || []).filter(function (h) { return h !== handler; });
      };
    }
    function onAny(handler) {
      anyListeners.push(handler);
      return function off() {
        var i = anyListeners.indexOf(handler);
        if (i >= 0) anyListeners.splice(i, 1);
      };
    }
    // Kernel state stream subscription. Receives `state_snapshot`
    // (once per (re)connect) then `state_event` per traffic + lifecycle
    // event. Refcounts: first call sends `state_subscribe` to the
    // server; last unsubscribe sends `state_unsubscribe`. The
    // subscribe frame is replayed on every reconnect so the new
    // connection produces a fresh snapshot — observers resync
    // without explicit reconnect handling.
    function subscribeState(handler) {
      stateHandlers.push(handler);
      if (stateHandlers.length === 1) sendFrame({ type: 'state_subscribe' });
      return function off() {
        var i = stateHandlers.indexOf(handler);
        if (i >= 0) stateHandlers.splice(i, 1);
        if (stateHandlers.length === 0) sendFrame({ type: 'state_unsubscribe' });
      };
    }
    // Lifecycle hook: fires 'connected' / 'disconnected' as the WS
    // transitions. Use to clear stale UI state when the server
    // restarts (e.g. an in-flight chat bubble that has no real
    // generation behind it any more).
    function onLifecycle(handler) {
      lifecycleHandlers.push(handler);
      return function off() {
        var i = lifecycleHandlers.indexOf(handler);
        if (i >= 0) lifecycleHandlers.splice(i, 1);
      };
    }

    connect();

    var dispatcher = new Proxy({}, {
      get: function (_t, target) {
        return function (payload) { return call(target, payload || {}); };
      }
    });

    // ─── browser-only bus (BroadcastChannel) ───
    // Same envelope shape as kernel: {type, target_id, source_id, ...}.
    // Universal — structured-clone payloads (bytes, objects, strings).
    // Bypasses kernel entirely; perfect for high-frequency UI traffic.
    var bcast = new BroadcastChannel('fantastic');
    var busListeners = {};
    var busAnyListeners = [];
    bcast.addEventListener('message', function (ev) {
      var d = ev.data || {};
      if (d.target_id && d.target_id !== agentId) return;  // not addressed to us
      if (d.source_id === agentId) return;                 // skip own echoes
      if (d.type && busListeners[d.type]) {
        busListeners[d.type].forEach(function (h) { try { h(d); } catch (e) {} });
      }
      busAnyListeners.forEach(function (h) { try { h(d.type, d); } catch (e) {} });
    });
    var bus = {
      send: function (target_id, payload) {
        bcast.postMessage(Object.assign({ source_id: agentId, target_id: target_id }, payload));
      },
      broadcast: function (payload) {
        bcast.postMessage(Object.assign({ source_id: agentId }, payload));
      },
      on: function (type, handler) {
        (busListeners[type] = busListeners[type] || []).push(handler);
        return function off() {
          busListeners[type] = (busListeners[type] || []).filter(function (h) { return h !== handler; });
        };
      },
      onAny: function (handler) {
        busAnyListeners.push(handler);
        return function off() {
          var i = busAnyListeners.indexOf(handler);
          if (i >= 0) busAnyListeners.splice(i, 1);
        };
      },
      close: function () { bcast.close(); },
    };

    // Universal reload signal. ANY agent can emit `{type:'reload_html'}`
    // on its own inbox and every page connected to it reloads. Used by
    // html_agent.set_html and the canvas frame reload button. The WS
    // proxy auto-watches the host agent on connect, so events on this
    // agent's inbox arrive without an explicit watch.
    on('reload_html', function () {
      try { location.reload(); } catch (e) {}
    });

    return {
      agentId: agentId,
      ready: ready,
      call: call,
      emit: emit,
      watch: watch,
      unwatch: unwatch,
      on: on,
      onAny: onAny,
      subscribeState: subscribeState,
      onLifecycle: onLifecycle,
      dispatcher: dispatcher,
      bus: bus,
    };
  };
})();
""".lstrip()
