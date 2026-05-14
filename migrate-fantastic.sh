#!/usr/bin/env bash
# migrate-fantastic.sh <PROJECT_DIR>
#
# Migrate a project's .fantastic/ to the post-rewire substrate:
#   1. backup → .fantastic_bak/
#   2. strip stale (cli/, "core.tools" handler_module, untyped agents)
#   3. rebundle webapp.tools → web.tools (collapse duplicates)
#   4. compose web_ws + web_rest as children of web (one-shot CLI calls)
#   5. verify: HTTP /, WS /core/ws call/reflect, REST /<rest>/_reflect
#   6. rollback on any verify failure
#
# Idempotent: re-running on an already-migrated dir is a no-op
# (steps detect and skip).

set -euo pipefail

PROJECT="${1:?usage: migrate-fantastic.sh <project-dir>}"
PROJECT="$(cd "$PROJECT" && pwd)"
cd "$PROJECT"

FANTASTIC_REPO=/Users/oleksandr/Projects/fantastic_canvas

echo "── migrating: $PROJECT"

# 0. Refuse if a live fantastic owns the dir.
if [ -f .fantastic/lock.json ]; then
  pid=$(python3 -c "import json; print(json.load(open('.fantastic/lock.json'))['pid'])" 2>/dev/null || echo "")
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    echo "  ✗ FAIL: live fantastic pid=$pid"
    exit 1
  fi
  rm -f .fantastic/lock.json
fi

# 1. Backup. Refuse to overwrite existing bak.
if [ ! -d .fantastic ]; then
  echo "  (no .fantastic dir — nothing to migrate)"
  exit 0
fi
if [ -d .fantastic_bak ]; then
  echo "  (.fantastic_bak already exists — assuming a prior run; continuing without re-backup)"
else
  cp -r .fantastic .fantastic_bak
  echo "  ✓ backup: .fantastic_bak"
fi

# 2. Strip stale records.
rm -rf .fantastic/agents/cli
python3 - <<'PY'
import json, pathlib
p = pathlib.Path('.fantastic/agent.json')
if p.exists():
    try:
        d = json.loads(p.read_text())
    except Exception:
        d = None
    if isinstance(d, dict) and d.pop('handler_module', None):
        p.write_text(json.dumps(d, indent=2))
        print('  ✓ stripped handler_module from root agent.json')
PY
python3 - <<'PY'
import json, pathlib, shutil
agents = pathlib.Path('.fantastic/agents')
if agents.exists():
    for d in sorted(agents.iterdir()):
        if not d.is_dir():
            continue
        af = d / 'agent.json'
        if not af.exists():
            continue
        try:
            rec = json.loads(af.read_text())
        except Exception:
            continue
        if not rec.get('handler_module'):
            print(f'  - dropping untyped {d.name}')
            shutil.rmtree(d)
PY

# 3. Rebundle webapp.tools → web.tools. Keep at most one (smallest id).
python3 - <<'PY'
import json, pathlib, shutil
agents = pathlib.Path('.fantastic/agents')
if not agents.exists():
    raise SystemExit
webapps = []
for d in sorted(agents.glob('webapp_*/')):
    af = d / 'agent.json'
    if not af.exists():
        continue
    try:
        rec = json.loads(af.read_text())
    except Exception:
        continue
    if rec.get('handler_module') == 'webapp.tools':
        webapps.append((d, rec))
if webapps:
    keep_dir, keep_rec = webapps[0]
    keep_rec['handler_module'] = 'web.tools'
    new_id = keep_dir.name.replace('webapp_', 'web_', 1)
    keep_rec['id'] = new_id
    new_dir = agents / new_id
    keep_dir.rename(new_dir)
    (new_dir / 'agent.json').write_text(json.dumps(keep_rec, indent=2))
    print(f'  ✓ rebundled {keep_dir.name} → {new_id}')
    for d, _ in webapps[1:]:
        print(f'  - dropping stale {d.name}')
        shutil.rmtree(d)
PY

# 4. Find the web agent.
WEB_ID=$(ls .fantastic/agents 2>/dev/null | grep '^web_' | head -1 || true)
if [ -z "$WEB_ID" ]; then
  echo "  (no web agent in tree — skipping surface composition + verify)"
  echo "  ✓ DONE: $PROJECT (state cleaned only)"
  exit 0
fi
echo "  WEB_ID=$WEB_ID"

# 5. Compose surfaces — one-shot CLI calls write records to disk. Idempotent.
if ls ".fantastic/agents/$WEB_ID/agents" 2>/dev/null | grep -q '^web_ws_'; then
  echo "  (web_ws already present — skipping)"
else
  uv run --project "$FANTASTIC_REPO" --active fantastic "$WEB_ID" create_agent handler_module=web_ws.tools >/dev/null
  echo "  ✓ web_ws added"
fi
if ls ".fantastic/agents/$WEB_ID/agents" 2>/dev/null | grep -q '^web_rest_'; then
  echo "  (web_rest already present — skipping)"
else
  uv run --project "$FANTASTIC_REPO" --active fantastic "$WEB_ID" create_agent handler_module=web_rest.tools >/dev/null
  echo "  ✓ web_rest added"
fi

# 6. Verify gate — boot daemon, probe HTTP + WS + REST, kill.
PORT=$(python3 -c "
import json, pathlib
p = pathlib.Path('.fantastic/agents/$WEB_ID/agent.json')
print(json.loads(p.read_text())['port'])
")
RID=$(ls ".fantastic/agents/$WEB_ID/agents" | grep '^web_rest_' | head -1)
LOG=/tmp/migrate-${PROJECT//\//_}.log
rm -f .fantastic/lock.json "$LOG"
uv run --project "$FANTASTIC_REPO" --active fantastic > "$LOG" 2>&1 &
for _ in $(seq 1 30); do grep -q "kernel up" "$LOG" 2>/dev/null && break; sleep 0.3; done

ok=true
if ! curl -fs "http://localhost:$PORT/" >/dev/null; then
  echo "  ✗ HTTP / failed"
  ok=false
fi
if ! curl -fs "http://localhost:$PORT/$RID/_reflect" >/dev/null; then
  echo "  ✗ REST /$RID/_reflect failed"
  ok=false
fi
if ! PORT="$PORT" uv run --project "$FANTASTIC_REPO" --active python - <<'PY'
import asyncio, json, os, websockets
port = os.environ["PORT"]
async def main():
    async with asyncio.timeout(5):
        async with websockets.connect(f"ws://localhost:{port}/core/ws") as ws:
            await ws.send(json.dumps({
                "type":"call","target":"core",
                "payload":{"type":"reflect"},"id":"1"
            }))
            while True:
                m = json.loads(await ws.recv())
                if m.get("id") == "1" and m.get("type") == "reply":
                    return
asyncio.run(main())
PY
then
  echo "  ✗ WS call/reflect failed"
  ok=false
else
  echo "  ✓ verify: HTTP + WS + REST all OK"
fi

pkill -9 -f "bin/fantastic" 2>/dev/null || true
sleep 0.3
rm -f .fantastic/lock.json

if [ "$ok" != "true" ]; then
  echo "  ✗ FAIL → rolling back"
  rm -rf .fantastic
  mv .fantastic_bak .fantastic
  exit 1
fi

echo "  ✓ DONE: $PROJECT"
