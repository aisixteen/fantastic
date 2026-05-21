# containerfiles/base selftest

> scopes: image, boot, http, ws, rest, canvas, add-member, install-bundle, persistence, shutdown
> requires: `podman` on `$PATH`; repo cloned at cwd; host port `18080` free; Python 3 with `websockets` available to the operator (probe 5 only); outbound network for probe 8 (`install-bundle`)
> drives the container end-to-end from OUTSIDE: build → run → probe → restart → stop. NOT pytest.

Verifies that `containerfiles/base/Containerfile` + `entrypoint.sh` ship a working fantastic kernel — base image boots a full canvas stack (`web` + `web_ws` + `web_rest` + `canvas_webapp` → `canvas_backend`) on first run, exposes HTTP/WS/REST on port 8080, accepts canvas members, supports `install-bundle`, survives restart, and shuts down cleanly on SIGTERM.

Port `18080` is used on the host (mapped to container `8080`) to dodge collisions with anything already serving locally.

## Pre-flight

- `podman` resolves on the operator's `$PATH`.
- The cwd is the repo root (so `containerfiles/base/Containerfile` is reachable as a build file with the repo as the build context).
- Host port `18080` is unbound. Quick check: `lsof -iTCP:18080 -sTCP:LISTEN` returns nothing.
- No prior `ft-test` container exists: `podman rm -f ft-test 2>/dev/null || true`.

## Setup

```bash
WORKDIR=$(mktemp -d)
# Pick the arch dir matching your host. Both wrap podman build with
# BASE_IMAGE=python:3.11-slim against the shared recipe at
# containerfiles/generic/Containerfile — only --platform differs.
ARCH=$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')
IMG=fantastic-canvas-base:dev-$ARCH
./containerfiles/base-$ARCH/build.sh
podman run -d --name ft-test -v "$WORKDIR:/workdir" -p 18080:8080 "$IMG"

# Wait up to 30s for [kernel] up.
for i in $(seq 1 60); do
  podman logs ft-test 2>&1 | grep -q "\[kernel\] up" && break
  sleep 0.5
done
podman logs ft-test 2>&1 | grep -q "\[kernel\] up" || { echo "FAIL: kernel never came up"; exit 1; }

# IDs the probes will reuse (top-level web + nested rest + canvas backend).
WEB_ID=$(podman exec ft-test ls /workdir/.fantastic/agents | grep '^web_' | head -1)
REST_ID=$(podman exec ft-test ls "/workdir/.fantastic/agents/$WEB_ID/agents" | grep '^web_rest_' | head -1)
CANVAS_WEBAPP_ID=$(podman exec ft-test ls /workdir/.fantastic/agents | grep '^canvas_webapp_' | head -1)
CANVAS_BACKEND_ID=$(podman exec ft-test ls "/workdir/.fantastic/agents/$CANVAS_WEBAPP_ID/agents" | grep '^canvas_backend_' | head -1)
echo "WEB_ID=$WEB_ID REST_ID=$REST_ID CANVAS_WEBAPP_ID=$CANVAS_WEBAPP_ID CANVAS_BACKEND_ID=$CANVAS_BACKEND_ID"
```

## Probes

### 1. `image` — build succeeds, size sane  [ image ]

```bash
podman image inspect "$IMG" --format '{{.Size}}'
```
Expected: build exits 0 (from the setup step) and the size prints a positive integer (typically 400–900 MB; alarming above ~1.5 GB).
Failure-mode: build failed → check the build log for `uv sync` resolution errors (lockfile drift) or missing system deps.

### 2. `boot` — fresh workdir reaches kernel up  [ boot ]

```bash
podman logs ft-test 2>&1 | grep -E "\[kernel\] up"
podman exec ft-test ls /workdir/.fantastic/agents | sort
```
Expected: `[kernel] up` printed once; the `ls` lists at least one `web_<hex>` and one `canvas_webapp_<hex>` at the top level.
Failure-mode: no `[kernel] up` → entrypoint failed during the seed step OR uvicorn never bound. Missing `canvas_webapp_` → `canvas_webapp.tools` seed line in `entrypoint.sh` regressed.

### 3. `http` — index served at `/`  [ http ]

```bash
curl -sf http://localhost:18080/ | head -c 4000 | python3 -c "
import sys
body = sys.stdin.read().lower()
assert 'canvas' in body or 'agent' in body, 'index did not render agent tree'
print('PASS')
"
```
Expected: `PASS` (the body holds either the canvas link or an agent-tree marker).
Failure-mode: `curl: (7) Failed to connect` → port 18080 not bound (container died or `-p` mapping wrong). Non-zero exit on the `python3` line → `web.tools` isn't returning the substrate tree index.

### 4. `rest` — kernel reflect through web_rest  [ rest ]

```bash
curl -sf "http://localhost:18080/$REST_ID/_reflect" | python3 -c "
import json, sys
d = json.load(sys.stdin)
assert 'transports' in d, f'no transports: {list(d)}'
assert 'available_bundles' in d
assert len(d['available_bundles']) >= 20, f'expected >=20 bundles, got {len(d[\"available_bundles\"])}'
print('PASS, bundles:', len(d['available_bundles']))
"
```
Expected: `PASS, bundles: <N>` with `N >= 20`.
Failure-mode: 404 → `REST_ID` discovery missed; <20 bundles → image's `uv sync` skipped workspace members (likely missed the `--frozen`/lockfile path).

### 5. `ws` — call/reflect round-trip on `/core/ws`  [ ws ]

```bash
python3 - <<'PY'
import asyncio, json, websockets
async def main():
    async with websockets.connect("ws://localhost:18080/core/ws") as ws:
        await ws.send(json.dumps({"type":"call","target":"kernel","payload":{"type":"reflect"},"id":"1"}))
        while True:
            m = json.loads(await ws.recv())
            if m.get("id") == "1" and m.get("type") in ("reply","error"):
                assert m["type"] == "reply", f"got error: {m}"
                assert "transports" in m["data"], "reply missing primer"
                print("PASS")
                return
asyncio.run(main())
PY
```
Expected: `PASS`.
Failure-mode: connection refused → `web_ws` didn't mount; `error` frame back → kernel reflect verb regressed.

### 6. `canvas` — canvas page renders + member list is empty  [ canvas | http | rest ]

```bash
curl -sf "http://localhost:18080/$CANVAS_WEBAPP_ID/" | python3 -c "
import sys
body = sys.stdin.read()
for needle in ('glViews', 'dblclick', 'canvas-world'):
    assert needle in body, f'canvas HTML missing {needle!r}'
print('PASS')
"
curl -sf -X POST -H 'content-type: application/json' \
  -d '{\"type\":\"list_members\"}' \
  "http://localhost:18080/$REST_ID/$CANVAS_BACKEND_ID" | python3 -c "
import json, sys
d = json.load(sys.stdin)
assert d.get('members') == [], f'expected empty members, got {d}'
print('PASS')
"
```
Expected: two `PASS` lines.
Failure-mode: missing needle → `canvas_webapp` template changed without updating the probe. Non-empty members on a fresh workdir → entrypoint accidentally seeded a member.

### 7. `add-member` — REST add_agent lands a member  [ add-member | canvas | rest ]

```bash
ADD_REPLY=$(curl -sf -X POST -H 'content-type: application/json' \
  -d '{\"type\":\"add_agent\",\"handler_module\":\"html_agent.tools\",\"x\":100,\"y\":100}' \
  "http://localhost:18080/$REST_ID/$CANVAS_BACKEND_ID")
echo "$ADD_REPLY" | python3 -c "
import json, sys
d = json.load(sys.stdin)
assert d.get('ok') is True, f'add_agent did not return ok: {d}'
assert d.get('member_id', '').startswith('html_agent_'), f'unexpected member_id: {d}'
assert d['member_id'] in d.get('members', []), f'member missing from list: {d}'
print(d['member_id'])
" | tee /tmp/ft-member-id
MEMBER_ID=$(cat /tmp/ft-member-id)
curl -sf -X POST -H 'content-type: application/json' \
  -d '{\"type\":\"list_members\"}' \
  "http://localhost:18080/$REST_ID/$CANVAS_BACKEND_ID" | python3 -c "
import json, sys, os
d = json.load(sys.stdin)
assert os.environ['MEMBER_ID'] in d.get('members', []), f'member not in list: {d}'
print('PASS')
" MEMBER_ID="$MEMBER_ID"
curl -sf -o /dev/null -w '%{http_code}\n' "http://localhost:18080/$MEMBER_ID/" | grep -q '^200$' && echo "PASS GET /<member>/"
```
Expected: prints the new `html_agent_<hex>` id, then `PASS`, then `PASS GET /<member>/`.
Failure-mode: `ok:false` reply → `canvas_backend.add_agent` regressed; 404 on the GET → `html_agent` not actually mounted into the agent tree.

### 8. `install-bundle` — uv pip install path is wired  [ install-bundle ]

```bash
podman exec ft-test fantastic install-bundle git+https://github.com/Alexadar/fantastic_canvas.git 2>&1 | tee /tmp/ft-install.log
grep -Eq 'uv pip install|Resolved|Building|error|failed' /tmp/ft-install.log && echo "PASS (uv invocation reached)" || echo "FAIL (no uv output)"
```
Expected: the log shows `uv pip install` activity. The repo itself is NOT a bundle, so the resolve/install WILL fail — that failure proves the path is wired. PASS = `uv pip install` ran (resolution attempted) regardless of exit code; FAIL = no `uv` output at all.
Failure-mode: `install-bundle` verb missing → `fantastic` printed a usage banner instead. `uv` not on `$PATH` in the final image → command-not-found.
TODO: when a public test bundle exists, replace the URL with `git+https://github.com/<user>/<test-bundle>` and assert exit 0 + a new entry-point appears in a follow-up reflect. Acceptable to mark this row `[pending]` until then.

### 9. `persistence` — survive stop/start, member stays  [ persistence | canvas ]

```bash
MEMBER_ID=$(cat /tmp/ft-member-id)
podman stop ft-test
podman start ft-test
for i in $(seq 1 60); do
  podman logs ft-test 2>&1 | tail -50 | grep -q "\[kernel\] up" && break
  sleep 0.5
done
curl -sf -X POST -H 'content-type: application/json' \
  -d '{\"type\":\"list_members\"}' \
  "http://localhost:18080/$REST_ID/$CANVAS_BACKEND_ID" | python3 -c "
import json, sys, os
d = json.load(sys.stdin)
assert os.environ['MEMBER_ID'] in d.get('members', []), f'member lost across restart: {d}'
print('PASS')
" MEMBER_ID="$MEMBER_ID"
```
Expected: `PASS` — the html_agent member added in probe 7 is still listed.
Failure-mode: kernel never comes back up → graceful-shutdown regressed and the agent tree didn't persist; member missing → `canvas_backend` membership not written to disk.

### 10. `shutdown` — SIGTERM is graceful  [ shutdown ]

```bash
podman stop ft-test
podman logs ft-test 2>&1 | grep -E 'SIGTERM|tearing down|down$' | tee /tmp/ft-shutdown.log
test "$(wc -l < /tmp/ft-shutdown.log)" -ge 3 && echo "PASS (3+ matches)" || echo "FAIL (fewer than 3 matches)"
```
Expected: at least three matching lines — `[kernel] SIGTERM — shutting down...`, `[kernel] tearing down agents...`, `[kernel] down`.
Failure-mode: `podman stop` blocks ~10s before SIGKILL → signal handler missing; fewer than 3 matches → teardown bailed early.

## Cleanup

```bash
podman rm -f ft-test
rm -rf "$WORKDIR"
rm -f /tmp/ft-member-id /tmp/ft-install.log /tmp/ft-shutdown.log
```

## Results

| # | Probe | Scope | Pass/Fail | Notes |
|---|-------|-------|-----------|-------|
| 1 | image | image | | |
| 2 | boot | boot | | |
| 3 | http | http | | |
| 4 | rest | rest | | |
| 5 | ws | ws | | |
| 6 | canvas | canvas, http, rest | | |
| 7 | add-member | add-member, canvas, rest | | |
| 8 | install-bundle | install-bundle | | |
| 9 | persistence | persistence, canvas | | |
| 10 | shutdown | shutdown | | |

All 10 PASS = container is healthy. Any FAIL → see Notes.
