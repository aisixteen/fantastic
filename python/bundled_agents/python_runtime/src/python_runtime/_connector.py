"""The kernel connector source injected into every spawned job.

`CONNECTOR_SRC` is a plain-Python IIFE-ish preamble (no imports leaking past
`_`-names) prepended to a job's code. It wraps the inherited control fd, spawns a
daemon reader thread to demux inbound frames (reply/event/inbox), and exposes a
kernel-mirroring `kernel` global. Frames are length-prefixed JSON (4-byte BE
length + body) — same convention as the WS binary frame header. If the channel
can't be set up, `kernel` degrades to a stub that raises a clear error, so a job
that doesn't touch the kernel still runs.

Kept as a string in its own module (imported by `tools.py`, read once at import)
rather than inline — it is exec'd in the JOB subprocess, never imported here.
"""

CONNECTOR_SRC = r"""# --- fantastic kernel connector (injected; talks to spawner over control fd) ---
try:
    import os as _o, json as _j, socket as _sk, struct as _st, threading as _th, queue as _q
    _ctrl = _sk.socket(fileno=int(_o.environ["FANTASTIC_CTRL_FD"]))
    _wlock = _th.Lock()
    _pending = {}
    _watchers = {}
    _inbox_cbs = set()
    _rid = [0]

    def _frame(obj):
        d = _j.dumps(obj).encode("utf-8")
        with _wlock:
            _ctrl.sendall(_st.pack(">I", len(d)) + d)

    def _recvn(n):
        b = b""
        while len(b) < n:
            c = _ctrl.recv(n - len(b))
            if not c:
                return None
            b += c
        return b

    def _reader():
        while True:
            h = _recvn(4)
            if h is None:
                break
            (n,) = _st.unpack(">I", h)
            body = _recvn(n)
            if body is None:
                break
            try:
                m = _j.loads(body.decode("utf-8"))
            except Exception:
                continue
            op = m.get("op")
            if op == "reply":
                q = _pending.pop(m.get("rid"), None)
                if q is not None:
                    q.put(m.get("data"))
            elif op == "event":
                for cb in list(_watchers.get(m.get("src"), ())):
                    try:
                        cb(m.get("payload"))
                    except Exception:
                        pass
            elif op == "inbox":
                for cb in list(_inbox_cbs):
                    try:
                        cb(m.get("payload"))
                    except Exception:
                        pass

    _th.Thread(target=_reader, daemon=True).start()

    class _Kernel:
        self_id = _o.environ.get("FANTASTIC_SELF_ID")

        def send(self, target, payload=None):
            with _wlock:
                _rid[0] += 1
                r = str(_rid[0])
            q = _q.Queue()
            _pending[r] = q
            _frame({"op": "send", "rid": r, "target": target, "payload": payload or {}})
            return q.get()

        def emit(self, target, payload=None):
            _frame({"op": "emit", "target": target, "payload": payload or {}})

        def reflect(self, target="kernel"):
            return self.send(target, {"type": "reflect"})

        def watch(self, src, cb):
            s = _watchers.get(src)
            if s is None:
                s = set()
                _watchers[src] = s
                _frame({"op": "watch", "src": src})
            s.add(cb)

            def _off():
                s.discard(cb)
                if not s:
                    _watchers.pop(src, None)
                    _frame({"op": "unwatch", "src": src})

            return _off

        def on_message(self, cb):
            if not _inbox_cbs:
                _frame({"op": "onmessage"})
            _inbox_cbs.add(cb)

            def _off():
                _inbox_cbs.discard(cb)
                if not _inbox_cbs:
                    _frame({"op": "offmessage"})

            return _off

        onMessage = on_message

    kernel = _Kernel()
except Exception as _e:
    class _Kernel:
        def _dead(self, *a, **k):
            raise RuntimeError("fantastic: kernel channel unavailable: %r" % (_e,))

        send = emit = reflect = watch = on_message = onMessage = _dead

    kernel = _Kernel()
# --- end connector ---
"""
