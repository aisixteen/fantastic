# containerfiles/base selftest

> scopes: image, boot, http, ws, rest, install-bundle, persistence, shutdown
> requires: `podman` on `$PATH`; repo cloned at cwd; host port `18080` free; Python 3 with `websockets` available to the operator (probe 5 only); outbound network for probe 7 (`install-bundle`)
> drives the container end-to-end from OUTSIDE: build → run → probe → restart → stop. NOT pytest.

Verifies that `containerfiles/base/Containerfile` + `entrypoint.sh` ship a working fantastic kernel — base image boots the host transport stack (`web` + `web_ws` + `web_rest`) on first run, exposes HTTP/WS/REST on port 8080, supports `install-bundle`, survives restart, and shuts down cleanly on SIGTERM. The host is pure data/compute/transport — the UI is the TS frontend kernel (`ts/`), served weakly and not exercised here.

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

# IDs the probes will reuse (top-level web + its nested ws/rest surfaces).
WEB_ID=$(podman exec ft-test ls /workdir/.fantastic/agents | grep '^web_' | head -1)
REST_ID=$(podman exec ft-test ls "/workdir/.fantastic/agents/$WEB_ID/agents" | grep '^web_rest_' | head -1)
WS_ID=$(podman exec ft-test ls "/workdir/.fantastic/agents/$WEB_ID/agents" | grep '^web_ws_' | head -1)
echo "WEB_ID=$WEB_ID REST_ID=$REST_ID WS_ID=$WS_ID"
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
podman exec ft-test ls "/workdir/.fantastic/agents/$WEB_ID/agents" | sort
```
Expected: `[kernel] up` printed once; the top-level `ls` lists one `web_<hex>`, and that web agent's own `agents/` holds a `web_ws_<hex>` and a `web_rest_<hex>`.
Failure-mode: no `[kernel] up` → entrypoint failed during the seed step OR uvicorn never bound. Missing `web_ws_`/`web_rest_` under the web agent → a `web_ws.tools` / `web_rest.tools` seed line in `entrypoint.sh` regressed.

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

### 4. `rest` — kernel reflect (bundle catalog) through web_rest  [ rest ]

`?bundles=all` composes the installable-bundle catalog into the reply
under `bundles` (the old top-level `available_bundles` key is gone —
reflect is now uniform, with the catalog behind the `bundles` flag).

```bash
curl -sf "http://localhost:18080/$REST_ID/_reflect?bundles=all" | python3 -c "
import json, sys
d = json.load(sys.stdin)
assert d.get('id') == 'core', f'id={d.get(\"id\")}'
assert 'tree' in d, f'no tree: {list(d)}'
assert 'bundles' in d, f'no bundles: {list(d)}'
assert len(d['bundles']) >= 20, f'expected >=20 bundles, got {len(d[\"bundles\"])}'
print('PASS, bundles:', len(d['bundles']))
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
                d = m["data"]
                assert d.get("id") == "core" and "tree" in d, f"reply not a uniform reflect: {list(d)}"
                print("PASS")
                return
asyncio.run(main())
PY
```
Expected: `PASS`.
Failure-mode: connection refused → `web_ws` didn't mount; `error` frame back → kernel reflect verb regressed.

### 6. `surfaces` — seeded transport children reflect as host agents  [ rest ]

The host is pure data/compute/transport — there is no `canvas_backend`
or any view/webapp bundle on it. The UI is the TS frontend kernel
(`ts/`), served weakly and federated over `web_ws`; it is not seeded by
this image and is not exercised here. The host-side check is that the
two call surfaces the entrypoint seeds under `web` — `web_ws` and
`web_rest` — are live host agents that answer `reflect`.

```bash
curl -sf "http://localhost:18080/$REST_ID/_reflect" | python3 -c "
import json, sys, os
d = json.load(sys.stdin)
assert d.get('id') == os.environ['REST_ID'], f'reflect id mismatch: {d.get(\"id\")}'
print('PASS web_rest reflect')
" REST_ID="$REST_ID"
curl -sf "http://localhost:18080/$REST_ID/$WS_ID" -X POST \
  -H 'content-type: application/json' -d '{\"type\":\"reflect\"}' | python3 -c "
import json, sys, os
d = json.load(sys.stdin)
assert d.get('id') == os.environ['WS_ID'], f'reflect id mismatch: {d.get(\"id\")}'
print('PASS web_ws reflect')
" WS_ID="$WS_ID"
```
Expected: `PASS web_rest reflect` then `PASS web_ws reflect`.
Failure-mode: 404 / id mismatch → a `web_ws.tools` / `web_rest.tools` seed line in `entrypoint.sh` regressed, or the surfaces didn't mount on the web app.

### 7. `install-bundle` — uv pip install path is wired  [ install-bundle ]

```bash
podman exec ft-test fantastic install-bundle git+https://github.com/Alexadar/fantastic_canvas.git 2>&1 | tee /tmp/ft-install.log
grep -Eq 'uv pip install|Resolved|Building|error|failed' /tmp/ft-install.log && echo "PASS (uv invocation reached)" || echo "FAIL (no uv output)"
```
Expected: the log shows `uv pip install` activity. The repo itself is NOT a bundle, so the resolve/install WILL fail — that failure proves the path is wired. PASS = `uv pip install` ran (resolution attempted) regardless of exit code; FAIL = no `uv` output at all.
Failure-mode: `install-bundle` verb missing → `fantastic` printed a usage banner instead. `uv` not on `$PATH` in the final image → command-not-found.
TODO: when a public test bundle exists, replace the URL with `git+https://github.com/<user>/<test-bundle>` and assert exit 0 + a new entry-point appears in a follow-up reflect. Acceptable to mark this row `[pending]` until then.

### 8. `persistence` — survive stop/start, seeded host agents stay  [ persistence ]

```bash
podman stop ft-test
podman start ft-test
for i in $(seq 1 60); do
  podman logs ft-test 2>&1 | tail -50 | grep -q "\[kernel\] up" && break
  sleep 0.5
done
# the web_rest surface seeded on first boot must still answer reflect.
curl -sf "http://localhost:18080/$REST_ID/_reflect" | python3 -c "
import json, sys, os
d = json.load(sys.stdin)
assert d.get('id') == os.environ['REST_ID'], f'web_rest lost across restart: {d}'
print('PASS')
" REST_ID="$REST_ID"
```
Expected: `PASS` — the `web_rest` surface seeded on first boot is still live after the restart.
Failure-mode: kernel never comes back up → graceful-shutdown regressed and the agent tree didn't persist; 404 / id mismatch → the seeded web stack wasn't written to disk.

### 9. `shutdown` — SIGTERM is graceful  [ shutdown ]

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
rm -f /tmp/ft-install.log /tmp/ft-shutdown.log
```

## Results

| # | Probe | Scope | Pass/Fail | Notes |
|---|-------|-------|-----------|-------|
| 1 | image | image | | |
| 2 | boot | boot | | |
| 3 | http | http | | |
| 4 | rest (kernel reflect + ?bundles=all catalog) | rest | | |
| 5 | ws | ws | | |
| 6 | surfaces (web_ws + web_rest reflect) | rest | | |
| 7 | install-bundle | install-bundle | | |
| 8 | persistence | persistence | | |
| 9 | shutdown | shutdown | | |

All 9 PASS = container is healthy. Any FAIL → see Notes.
