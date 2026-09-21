"""The kernel connector source injected into every spawned job.

`CONNECTOR_SRC` is a plain-Python IIFE-ish preamble (no imports leaking past
`_`-names) prepended to a job's code. It wraps the inherited control fd, spawns a
daemon reader thread to demux inbound frames (reply/event/inbox), and exposes a
kernel-mirroring `kernel` global. Frames are `[4-byte BE length | kind | rest]`:
kind `T` = a UTF-8 JSON envelope; kind `B` = the io_bridge binary frame
`[4-byte BE H | H-byte JSON header | raw body]`, the header naming in
`_binary_path` where the body belongs (a `read_stream` chunk, a `write_stream`
payload) — raw bytes ride the fd raw, never base64. One frame caps at 16 MiB,
the bound every relay reads with; over it `kernel.send`/`emit` raises rather
than writing a frame the relay would close the socket over. If the channel
can't be set up, `kernel` degrades to a stub that raises a clear error, so a job
that doesn't touch the kernel still runs.

Kept as a string in its own module (imported by `tools.py`, read once at import)
rather than inline — it is exec'd in the JOB subprocess, never imported here.
"""

CONNECTOR_SRC = r"""# --- fantastic kernel connector (injected; talks to spawner over control fd) ---
try:
    import os as _o, json as _j, socket as _sk, struct as _st, threading as _th, queue as _q
    import sys as _sys, copy as _cp
    _ctrl = _sk.socket(fileno=int(_o.environ["FANTASTIC_CTRL_FD"]))
    _wlock = _th.Lock()
    _pending = {}
    _watchers = {}
    _inbox_cbs = set()
    _rid = [0]
    # ONE frame bound, both ends, every runtime. Over it the RELAY treats the
    # length as a wire violation and closes the socket — which the reader below
    # reads as "the kernel is gone" and reaps the process. So refuse HERE, where
    # the caller still has a stack to see: a 20 MB payload is a stream, not a
    # frame.
    _MAX_FRAME = 16 * 1024 * 1024

    def _bytes_at(obj, prefix=""):
        # The first bytes value, depth-first: (dotted path, value) or None.
        if isinstance(obj, (bytes, bytearray)):
            return (prefix, bytes(obj))
        if isinstance(obj, dict):
            for k, v in obj.items():
                r = _bytes_at(v, prefix + "." + k if prefix else k)
                if r is not None:
                    return r
        elif isinstance(obj, list):
            for i, v in enumerate(obj):
                r = _bytes_at(v, prefix + "." + str(i) if prefix else str(i))
                if r is not None:
                    return r
        return None

    def _set_at(obj, path, value):
        parts = path.split(".")
        for p in parts[:-1]:
            obj = obj[int(p)] if isinstance(obj, list) else obj[p]
        if isinstance(obj, list):
            obj[int(parts[-1])] = value
        else:
            obj[parts[-1]] = value

    def _frame(obj):
        # Kind T: JSON. Kind B: a payload carrying bytes rides as the io_bridge
        # binary frame — header with null at `_binary_path`, raw body after it.
        hit = _bytes_at(obj)
        if hit is None:
            d = b"T" + _j.dumps(obj).encode("utf-8")
        else:
            path, body = hit
            head = _cp.deepcopy(obj)
            _set_at(head, path, None)
            head["_binary_path"] = path
            h = _j.dumps(head).encode("utf-8")
            d = b"B" + _st.pack(">I", len(h)) + h + body
        if len(d) > _MAX_FRAME:
            raise ValueError(
                "fantastic: frame exceeds 16 MiB — stream it in chunks (write_stream)"
            )
        with _wlock:
            _ctrl.sendall(_st.pack(">I", len(d)) + d)

    def _unframe(body):
        if body[:1] == b"B":
            (hl,) = _st.unpack(">I", body[1:5])
            m = _j.loads(body[5:5 + hl].decode("utf-8"))
            p = m.pop("_binary_path", None)
            if p is not None:
                _set_at(m, p, body[5 + hl:])
            return m
        if body[:1] == b"T":
            return _j.loads(body[1:].decode("utf-8"))
        raise ValueError("unknown frame kind")

    def _recvn(n):
        b = b""
        while len(b) < n:
            c = _ctrl.recv(n - len(b))
            if not c:
                return None
            b += c
        return b

    _dispatch_q = _q.Queue()

    def _dispatcher():
        # watch/on_message callbacks run HERE, on a dedicated thread OFF the
        # reader — so a callback may freely call kernel.send/emit (blocking on a
        # reply) without deadlocking: the reader thread stays free to read that
        # reply. Single thread ⇒ callbacks stay strictly ordered.
        while True:
            cbs, payload = _dispatch_q.get()
            for cb in cbs:
                try:
                    cb(payload)
                except Exception:
                    # A callback that raises is a CRASHED program, not a skipped
                    # message. Swallowing it parked the process forever looking
                    # merely deaf — the one failure the author/driver cannot
                    # diagnose. Die loudly: the traceback lands on stderr and the
                    # parent reports job_done {ok:false, stderr:<it>}.
                    import traceback as _tb

                    _tb.print_exc()
                    _o._exit(1)

    _th.Thread(target=_dispatcher, daemon=True).start()

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
                m = _unframe(body)
            except Exception:
                continue
            op = m.get("op")
            if op == "reply":
                q = _pending.pop(m.get("rid"), None)
                if q is not None:
                    q.put(m)
            elif op == "event":
                _dispatch_q.put((list(_watchers.get(m.get("src"), ())), m.get("payload")))
            elif op == "inbox":
                _dispatch_q.put((list(_inbox_cbs), m.get("payload")))
        # EOF on the control socket ⇒ the spawner (kernel) is GONE. A durable tool
        # parked on its inbox (threading.Event().wait(), an on_message loop) would
        # otherwise linger as an ORPHAN forever — the lifeline is severed, there is
        # no kernel left to serve. Reap the whole process, not just this daemon
        # thread. This is what makes a kernel exit (parent dies → socket closes)
        # cascade-reap every durable job even when the parent dies too abruptly to
        # run its own proc.kill() loop (the orphaned connectors the audit found).
        # A short compute job exits via its main thread BEFORE any EOF, so this
        # only ever fires for a job still alive when its kernel left.
        _o._exit(0)

    _th.Thread(target=_reader, daemon=True).start()

    class _Kernel:
        # The OWNER agent's id — the tool's public id, not the ephemeral job id.
        # A general-purpose command runner takes its arguments as ARGV, like any
        # other command: there is no per-call args object here, and no spawner in
        # any runtime ever set the env var one would have ridden in.
        self_id = _o.environ.get("FANTASTIC_SELF_ID")

        def send(self, target, payload=None):
            with _wlock:
                _rid[0] += 1
                r = str(_rid[0])
            q = _q.Queue()
            _pending[r] = q
            try:
                _frame(
                    {"op": "send", "rid": r, "target": target, "payload": payload or {}}
                )
            except Exception:
                _pending.pop(r, None)
                raise
            m = q.get()
            if "error" in m:
                # The RELAY could not deliver the reply (as opposed to the target
                # answering with its own {error} — that is data, returned as-is).
                # A silent None here cost a builder 50 messages (measured 2026-09-02).
                raise RuntimeError(
                    "fantastic: send(%r) failed in transit: %s" % (target, m["error"])
                )
            return m.get("data")

        def emit(self, payload_or_target, payload=None):
            # One-arg `emit(payload)` emits on the OWNER agent (the common form,
            # parity with the lua/js runtimes + the genesis contract). Two-arg
            # `emit(target, payload)` emits on a named target (back-compat).
            if payload is None:
                _frame({"op": "emit", "payload": payload_or_target or {}})
            else:
                _frame(
                    {"op": "emit", "target": payload_or_target, "payload": payload or {}}
                )

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
    # A model that sees `kernel.send(...)` in the docs often writes the natural
    # idiom `import kernel`. Without this that would rebind the name to the
    # substrate `kernel` PACKAGE (installed in the venv), silently breaking the
    # tool. Register the injected connector under that name so `import kernel`
    # returns the connector (same fix as a `require('kernel')` binding). Safe:
    # the job runs in its OWN subprocess, so this doesn't touch the host's modules.
    _sys.modules["kernel"] = kernel
except Exception as _e:
    class _Kernel:
        def _dead(self, *a, **k):
            raise RuntimeError("fantastic: kernel channel unavailable: %r" % (_e,))

        send = emit = reflect = watch = on_message = onMessage = _dead

    kernel = _Kernel()
    try:
        import sys as _sys

        _sys.modules["kernel"] = kernel
    except Exception:
        pass
# --- end connector ---
"""
