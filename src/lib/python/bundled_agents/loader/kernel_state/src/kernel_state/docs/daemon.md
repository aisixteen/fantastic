> Reach a running daemon's live surface, stop the kernel process, terminal first-contact.

# Reach this kernel

Check `.fantastic/lock.yaml` first — it holds `{pid}`. One kernel per dir,
so the two paths are mutually exclusive:

**A daemon IS running** (it holds the flock on lock.yaml, which names its
pid). The one-shot CLI is locked out — go through the daemon's surface. Find it in the tree: a
`web.tools` node carries the HTTP `port`; its children are the surfaces.
  - `web_rest.tools` child → `POST http://localhost:<port>/<rest_id>/<target>`
    body=`{"type":"<verb>",...}`; or browser-pastable
    `GET http://localhost:<port>/<rest_id>/_reflect[/<target>][?readme=1]`
  - `web_ws.tools` child → `ws://localhost:<port>/<agent>/ws`
    (frames: `{"type":"call","target":..,"payload":..,"id":..}`)
  To get the tree itself: `GET http://localhost:<port>/<rest_id>/_reflect`.

**No daemon running** (nobody holds that flock — a lock.yaml left behind is
stale, even when its pid is alive again). The one-shot CLI works — each call
takes the lock, spawns a fresh kernel, reads disk, dispatches, exits:
  - `fantastic reflect` — the agent tree (add `bundles=all` for the catalog)
  - `fantastic <id> <verb> [k=v ...]` — call any verb on any agent
  (one-shots don't see live process-memory state — they're disk-only.)

**A daemon is running but there's no `web.tools` node** → no HTTP surface
and one-shots are locked out. Tell the user to add one (don't guess a port
yourself):

    fantastic kernel create_agent parent_id=kernel_state handler_module=web.tools port=<N>
    fantastic kernel create_agent parent_id=<web_id> handler_module=web_ws.tools
    fantastic kernel create_agent parent_id=<web_id> handler_module=web_rest.tools
    fantastic kernel create_agent parent_id=<web_id> handler_module=kernel_state.tools \
        id=web_loader root=.fantastic/web watch=false   # frontend store (readme=frontend)
    fantastic web_loader persist_record \
        record='{"id":"<root_id>","handler_module":"<frontend-root>.ts"}'  # seed the frontend

...then they restart the daemon.

# Stopping the kernel

`send kernel {"type":"kernel_stop"}` gracefully stops the whole kernel
PROCESS. It acks `{"type":"kernel_stop","ok":true}` FIRST, then (after the
reply is on the wire) drains: new work is refused, every call settles (done,
or cancelled into the dead letters), the last execution rows flush into the
kernel's memory store, agents shut down, `.fantastic/lock.yaml` releases, the
HTTP/WS listeners stop, and it exits code 0. Reachable over both
`web_rest` (POST) and `web_ws`, so a remote operator can stop the kernel
with one verb — no `kill`, no backend knowledge.

This is **root-only**: it is answered by the kernel's own control surface
(the tree root, address `kernel`), gated like any call there; sending it to any child
agent returns an error. It is **backend-agnostic**: the kernel is PID 1 in a
container, so its exit stops the container (auto-removed if run with `--rm`);
a bare host process simply dies. Either way the port goes down and the lock
releases. Idempotent / one-shot: a second call lands on a dead port. The
bind-mounted `.fantastic/` workdir persists across the stop.

# First contact — if you arrived via the terminal

If a `fantastic` daemon was launched on a tty (or you are an LLM in an
`exec_bridge` PTY job here), it greeted you with a TWO-PHASE intro: first
`[fantastic] … — booting…` (identity + a compact PULL/PUSH control-plane
map), then `[kernel] up` (the live REST/WS attach coordinates, or a compose
hint if no web surface exists yet). That banner is the SHORT map; the doc
tree (`reflect readme=index`) is the full one it pointed you to.
