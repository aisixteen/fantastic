#!/bin/sh
# Build smoke test — validates the universal container BUILD + run contract.
# SEPARATE from the main test suites (not pytest/npm/node --test): it exercises
# the built image, not kernel logic.
#
#   sh container/test/build_smoke.sh          # build + test
#   BUILD=0 sh container/test/build_smoke.sh  # test an already-built image
#
# Skips cleanly if neither podman nor docker is present.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH= cd -- "$HERE/../.." && pwd)
TAG="${TAG:-fantastic:latest}"
BUILD="${BUILD:-1}"

if command -v podman >/dev/null 2>&1; then ENGINE=podman
elif command -v docker >/dev/null 2>&1; then ENGINE=docker
else echo "SKIP: no podman/docker"; exit 0; fi
echo "== engine: $ENGINE / tag: $TAG =="

PASS=0; FAIL=0
ok()  { echo "  PASS  $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL  $1"; FAIL=$((FAIL+1)); }

CONTAINERS=""
cleanup() { for c in $CONTAINERS; do $ENGINE rm -f "$c" >/dev/null 2>&1 || true; done; }
trap cleanup EXIT INT TERM

freeport() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'; }

# The image performs NO agent autocreation — the OPERATOR composes the web stack.
# These one-shots (entrypoint bypassed) seed web+web_ws+rest into a workdir, the
# way a project/LLM would, before the daemon boots it. $1=workdir $2=bin $3=root $4=port
PYBIN=/opt/fantastic/venv/bin/fantastic
RUSTBIN=/opt/fantastic/bin/fantastic-rust
compose_web() {
  $ENGINE run --rm -v "$1:/work" -w /work --entrypoint "$2" "$TAG" "$3" create_agent handler_module=web.tools id=web "port=$4" >/dev/null 2>&1
  $ENGINE run --rm -v "$1:/work" -w /work --entrypoint "$2" "$TAG" web create_agent handler_module=web_ws.tools id=web_ws >/dev/null 2>&1
  $ENGINE run --rm -v "$1:/work" -w /work --entrypoint "$2" "$TAG" web create_agent handler_module=web_rest.tools id=rest >/dev/null 2>&1
}

# ── build ──────────────────────────────────────────────────────────────────
if [ "$BUILD" = 1 ]; then
  echo "== build =="
  sh "$REPO/container/build.sh"
fi

# ── runtime-field check (one-shot reflect, no daemon) ───────────────────────
echo "== reflect.runtime per runtime =="
for pair in "python:/opt/fantastic/venv/bin/fantastic" "rust:/opt/fantastic/bin/fantastic-rust"; do
  rt=${pair%%:*}; bin=${pair#*:}
  out=$($ENGINE run --rm --entrypoint "$bin" "$TAG" reflect 2>/dev/null || true)
  if printf '%s' "$out" | grep -Eq "\"runtime\"[[:space:]]*:[[:space:]]*\"$rt\""; then
    ok "$rt kernel reports runtime=\"$rt\""
  else
    bad "$rt kernel reflect missing runtime=\"$rt\" (got: $(printf '%s' "$out" | head -c 120))"
  fi
done

# ── serve check (daemon up, reachable via -p, workdir written) ──────────────
echo "== serve (bind 0.0.0.0 + -p mapping + workdir) =="
for rt in python rust; do
  P=$(freeport); tmp=$(mktemp -d); c="ftsmoke-$rt-$$"
  CONTAINERS="$CONTAINERS $c"
  case "$rt" in python) bin=$PYBIN; root=fs_loader ;; rust) bin=$RUSTBIN; root=core ;; esac
  compose_web "$tmp" "$bin" "$root" "$P"   # explicit composition (image autocreates nothing)
  $ENGINE run -d --name "$c" -p "127.0.0.1:$P:$P" -v "$tmp:/work" \
    -e FANTASTIC_RUNTIME="$rt" -e FANTASTIC_PORT="$P" "$TAG" >/dev/null
  up=""
  i=0; while [ $i -lt 100 ]; do
    if curl -sf -o /dev/null "http://127.0.0.1:$P/"; then up=1; break; fi
    sleep 0.5; i=$((i+1))
  done
  if [ -n "$up" ]; then ok "$rt daemon serves http://127.0.0.1:$P/ (200)"
  else bad "$rt daemon never bound :$P ($($ENGINE logs "$c" 2>&1 | tail -3 | tr '\n' '|'))"; fi
  # Call a verb over the wire (web_rest surface) — proves the kernel is callable,
  # not just rendering, and dispatch works end-to-end.
  rj=$(curl -s -X POST -H 'Content-Type: application/json' "http://127.0.0.1:$P/rest/kernel" -d '{"type":"reflect"}' 2>/dev/null || true)
  printf '%s' "$rj" | grep -Eq "\"runtime\"[[:space:]]*:[[:space:]]*\"$rt\"" \
    && ok "$rt reflect over REST (POST /rest/kernel) → runtime=\"$rt\"" \
    || bad "$rt reflect-over-REST failed (got: $(printf '%s' "$rj" | head -c 100))"
  # Deployment context baked into the image: the kernel reflects env="container"
  # and a non-null version (FANTASTIC_ENV / FANTASTIC_VERSION inherited from ENV).
  printf '%s' "$rj" | grep -Eq "\"env\"[[:space:]]*:[[:space:]]*\"container\"" \
    && ok "$rt reflect → env=\"container\" (deployment context)" \
    || bad "$rt reflect missing env=\"container\" (got: $(printf '%s' "$rj" | head -c 120))"
  printf '%s' "$rj" | grep -Eq "\"version\"[[:space:]]*:[[:space:]]*\"[^\"]+\"" \
    && ok "$rt reflect → version present (build tag baked in)" \
    || bad "$rt reflect missing a non-null version (got: $(printf '%s' "$rj" | head -c 120))"
  [ -d "$tmp/.fantastic" ] && ok "$rt wrote /work/.fantastic (workdir bind ok)" \
    || bad "$rt did not write /work/.fantastic"
  # SIGTERM-clean: stop should return promptly (tini forwards; no 10s kill wait).
  t0=$(date +%s); $ENGINE stop -t 8 "$c" >/dev/null 2>&1 || true; t1=$(date +%s)
  [ $((t1 - t0)) -lt 8 ] && ok "$rt stops promptly on SIGTERM ($((t1-t0))s)" \
    || bad "$rt did not stop within grace (SIGTERM not handled?)"
  $ENGINE rm -f "$c" >/dev/null 2>&1 || true; rm -rf "$tmp"
done

# ── graceful self-shutdown verb (the backend-agnostic stop) ─────────────────
# `send kernel {type:shutdown_kernel}` over REST → the kernel acks, then exits 0.
# The kernel is PID 1 (under tini), so its exit STOPS the container — and with
# `--rm` the container AUTO-REMOVES. The bind-mounted /work/.fantastic persists.
# This is the whole point: the app stops a kernel with one verb, container or not.
echo "== shutdown_kernel (--rm container stops + auto-removes; workdir persists) =="
P=$(freeport); tmp=$(mktemp -d); c="ftsmoke-shutdown-$$"
CONTAINERS="$CONTAINERS $c"
compose_web "$tmp" "$PYBIN" fs_loader "$P"
$ENGINE run -d --rm --name "$c" -p "127.0.0.1:$P:$P" -v "$tmp:/work" \
  -e FANTASTIC_RUNTIME=python -e FANTASTIC_PORT="$P" "$TAG" >/dev/null
up=""; i=0; while [ $i -lt 100 ]; do curl -sf -o /dev/null "http://127.0.0.1:$P/" && { up=1; break; }; sleep 0.5; i=$((i+1)); done
if [ -z "$up" ]; then
  bad "shutdown_kernel: container never bound :$P ($($ENGINE logs "$c" 2>&1 | tail -2 | tr '\n' '|'))"
else
  # lock.json is present while up → the post-shutdown persistence/exit is a
  # real transition, not a vacuous pass.
  [ -f "$tmp/.fantastic/lock.json" ] && ok "lock.json present while the kernel is up" \
    || bad "lock.json missing while the kernel is up"
  ack=$(curl -s -X POST -H 'Content-Type: application/json' "http://127.0.0.1:$P/rest/kernel" -d '{"type":"shutdown_kernel"}' 2>/dev/null || true)
  printf '%s' "$ack" | grep -q '"ok"[[:space:]]*:[[:space:]]*true' \
    && ok "shutdown_kernel acked {ok:true} over REST" \
    || bad "shutdown_kernel ack missing (got: $(printf '%s' "$ack" | head -c 80))"
  # CORE assertion: the kernel exited → PID 1 gone → container STOPPED. Robust
  # to --rm timing: inspect reporting Running=false OR failing (already
  # auto-removed) both mean stopped. Decoupled from the async auto-remove below.
  stopped=""; i=0; while [ $i -lt 60 ]; do
    st=$($ENGINE inspect -f '{{.State.Running}}' "$c" 2>/dev/null) || { stopped=1; break; }
    [ "$st" = "false" ] && { stopped=1; break; }
    sleep 0.5; i=$((i+1))
  done
  [ -n "$stopped" ] && ok "shutdown_kernel → kernel exited (container stopped)" \
    || bad "container still running after shutdown_kernel (state: $($ENGINE inspect -f '{{.State.Running}}' "$c" 2>&1))"
  # Secondary: --rm auto-removes the stopped container (async; own ceiling).
  removed=""; i=0; while [ $i -lt 40 ]; do
    $ENGINE inspect "$c" >/dev/null 2>&1 || { removed=1; break; }
    sleep 0.5; i=$((i+1))
  done
  [ -n "$removed" ] && ok "--rm auto-removed the stopped container" \
    || bad "container not auto-removed under --rm (still listed)"
  [ -d "$tmp/.fantastic" ] && ok "bind-mounted /work/.fantastic persists after shutdown" \
    || bad "workdir .fantastic vanished after shutdown_kernel"
fi
rm -rf "$tmp"

# ── embedded JS zip discoverable + pull-revivable (no engine) ───────────────
echo "== embedded js_kernel.zip =="
zout=$($ENGINE run --rm --entrypoint sh "$TAG" -c \
  'unzip -p "$FANTASTIC_JS_KERNEL_ZIP" readme.md 2>/dev/null | head -c 80' 2>/dev/null || true)
printf '%s' "$zout" | grep -qi "fantastic" \
  && ok "js_kernel.zip present; 'unzip -p … readme.md' works inside the image" \
  || bad "js_kernel.zip readme not pullable (got: $(printf '%s' "$zout" | head -c 60))"

# ── security/slim: no JS engine, no compilers in the final image ────────────
echo "== no JS engine / compilers =="
found=$($ENGINE run --rm --entrypoint sh "$TAG" -c \
  'for b in node bun deno cargo go rustc gcc cc; do command -v "$b" 2>/dev/null && echo "FOUND:$b"; done; true' 2>/dev/null || true)
if printf '%s' "$found" | grep -q FOUND; then
  bad "engine/compiler present in final image: $(printf '%s' "$found" | tr '\n' ' ')"
else
  ok "no node/bun/deno/cargo/go/rustc/gcc in the final image"
fi

# ── head: a composed web serves the all-readmes page at / (head on by default) ─
# The image autocreates nothing, so compose a python web on :8088 first; with
# head on (default) its / serves the head page.
echo "== head served by a composed web (default on) + FANTASTIC_HEAD=off fallback =="
P=$(freeport); tmp=$(mktemp -d); c="ftsmoke-head-$$"
CONTAINERS="$CONTAINERS $c"
compose_web "$tmp" "$PYBIN" fs_loader 8088
$ENGINE run -d --name "$c" -p "127.0.0.1:$P:8088" -v "$tmp:/work" "$TAG" >/dev/null
up=""; i=0; while [ $i -lt 100 ]; do curl -sf -o /dev/null "http://127.0.0.1:$P/" && { up=1; break; }; sleep 0.5; i=$((i+1)); done
page=$(curl -s "http://127.0.0.1:$P/" 2>/dev/null || true)
if [ -n "$up" ] && printf '%s' "$page" | grep -q "KERNEL HEAD" \
   && printf '%s' "$page" | grep -q "Aisixteen Fantastic"; then
  ok "default run serves the all-readmes head at / (no flags; host :$P → :8088)"
else
  bad "default head page missing/incomplete ($($ENGINE logs "$c" 2>&1 | tail -2 | tr '\n' '|'))"
fi
hr=$(curl -s -X POST -H 'Content-Type: application/json' "http://127.0.0.1:$P/rest/kernel" -d '{"type":"reflect"}' 2>/dev/null || true)
printf '%s' "$hr" | grep -Eq '"runtime"[[:space:]]*:[[:space:]]*"python"' \
  && ok "head endpoint is also a live kernel (reflect over REST → python)" \
  || bad "head reflect-over-REST failed"
$ENGINE rm -f "$c" >/dev/null 2>&1 || true; rm -rf "$tmp"

# FANTASTIC_HEAD=off → / falls back to the plain agent-tree index (no head page).
P=$(freeport); tmp=$(mktemp -d); c="ftsmoke-nohead-$$"
CONTAINERS="$CONTAINERS $c"
compose_web "$tmp" "$PYBIN" fs_loader 8088
$ENGINE run -d --name "$c" -p "127.0.0.1:$P:8088" -v "$tmp:/work" \
  -e FANTASTIC_HEAD=off "$TAG" >/dev/null
up=""; i=0; while [ $i -lt 100 ]; do curl -sf -o /dev/null "http://127.0.0.1:$P/" && { up=1; break; }; sleep 0.5; i=$((i+1)); done
page=$(curl -s "http://127.0.0.1:$P/" 2>/dev/null || true)
if [ -n "$up" ] && ! printf '%s' "$page" | grep -q "KERNEL HEAD"; then
  ok "FANTASTIC_HEAD=off drops the head → plain agent index at /"
else
  bad "FANTASTIC_HEAD=off still served the head page (flag ignored?)"
fi
$ENGINE rm -f "$c" >/dev/null 2>&1 || true; rm -rf "$tmp"

# rust also honours the head hook (FANTASTIC_WEB_INDEX in axum) for a composed web.
P=$(freeport); tmp=$(mktemp -d); c="ftsmoke-rusthead-$$"
CONTAINERS="$CONTAINERS $c"
compose_web "$tmp" "$RUSTBIN" core 8088
$ENGINE run -d --name "$c" -p "127.0.0.1:$P:8088" -v "$tmp:/work" \
  -e FANTASTIC_RUNTIME=rust "$TAG" >/dev/null
up=""; i=0; while [ $i -lt 100 ]; do curl -sf -o /dev/null "http://127.0.0.1:$P/" && { up=1; break; }; sleep 0.5; i=$((i+1)); done
page=$(curl -s "http://127.0.0.1:$P/" 2>/dev/null || true)
if [ -n "$up" ] && printf '%s' "$page" | grep -q "KERNEL HEAD"; then
  ok "rust runtime also serves the head at / by default"
else
  bad "rust head page missing ($($ENGINE logs "$c" 2>&1 | tail -2 | tr '\n' '|'))"
fi
$ENGINE rm -f "$c" >/dev/null 2>&1 || true; rm -rf "$tmp"

# ── no agent autocreation: a BLANK workdir gets nothing composed ────────────
echo "== no agent autocreation (blank workdir) =="
tmp=$(mktemp -d); c="ftsmoke-noauto-$$"; CONTAINERS="$CONTAINERS $c"
out=$($ENGINE run --name "$c" -v "$tmp:/work" "$TAG" 2>&1 || true)   # boots, no web → exits
printf '%s' "$out" | grep -q "composes nothing" \
  && ok "blank workdir → entrypoint composes nothing (prints the note)" \
  || bad "expected 'composes nothing' note (got: $(printf '%s' "$out" | tail -3 | tr '\n' '|'))"
if grep -rqs '"handler_module"[[:space:]]*:[[:space:]]*"web.tools"' "$tmp/.fantastic/agents" 2>/dev/null; then
  bad "a web.tools agent was AUTOCREATED in a blank workdir (autocreation not removed!)"
else
  ok "no web/web_ws/rest agent autocreated in the blank workdir"
fi
$ENGINE rm -f "$c" >/dev/null 2>&1 || true; rm -rf "$tmp"

echo "== result: $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
