# --- fantastic kernel connector (injected; talks to spawner over control fd) ---
try:
    import os as _o, json as _j, socket as _sk, struct as _st, threading as _th, queue as _q
    import sys as _sys
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
                m = _j.loads(body.decode("utf-8"))
            except Exception:
                continue
            op = m.get("op")
            if op == "reply":
                q = _pending.pop(m.get("rid"), None)
                if q is not None:
                    q.put(m.get("data"))
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
        self_id = _o.environ.get("FANTASTIC_SELF_ID")
        # THIS CALL'S arguments — whatever the caller put in `start`'s `args` field.
        # Every other agent takes its arguments in the payload; without this the
        # script executor is the one place the payload is discarded, so an authored
        # tool can only know things that were baked into its `code` or stored on its
        # record. With it the tool is a pure function: the caller passes the inputs
        # AND names where the result goes, so one tool serves many callers and fans
        # out for free. `{}` when the caller passed none.
        args = _j.loads(_o.environ.get("FANTASTIC_ARGS") or "{}")

        def send(self, target, payload=None):
            with _wlock:
                _rid[0] += 1
                r = str(_rid[0])
            q = _q.Queue()
            _pending[r] = q
            _frame({"op": "send", "rid": r, "target": target, "payload": payload or {}})
            return q.get()

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
    # returns the connector (mirrors the js_runtime `require('kernel')` fix). Safe:
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
