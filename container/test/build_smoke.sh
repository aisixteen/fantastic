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
# way a project/LLM would, before the daemon boots it. The call legs (web_ws,
# web_rest) are io_bridge derivations — SEALED by default — so we open them with
# ingress_rule=allow_all, exactly as the operator must. $1=workdir $2=bin $3=root $4=port
PYBIN=/opt/fantastic/venv/bin/fantastic_kernel
RUSTBIN=/opt/fantastic/bin/fantastic-rust
SWIFTBIN=/opt/fantastic/bin/fantastic-swift
compose_web() {
  # Wire the persistence PROVIDER first — a file_bridge@.fantastic (open). Python
  # AND rust auto-persist records ONLY through a discovered store now (RAM if
  # unwired), so without this the web/web_ws/rest one-shots would never reach disk
  # and the daemon would boot empty. The store self-persists (survives to the next
  # one-shot). Idempotent across the calls below.
  $ENGINE run --rm -v "$1:/work" -w /work --entrypoint "$2" "$TAG" "$3" create_agent handler_module=file_bridge.tools id=store root=.fantastic ingress_rule=allow_all >/dev/null 2>&1
  $ENGINE run --rm -v "$1:/work" -w /work --entrypoint "$2" "$TAG" "$3" create_agent handler_module=web.tools id=web "port=$4" >/dev/null 2>&1
  $ENGINE run --rm -v "$1:/work" -w /work --entrypoint "$2" "$TAG" web create_agent handler_module=web_ws.tools id=web_ws ingress_rule=allow_all >/dev/null 2>&1
  $ENGINE run --rm -v "$1:/work" -w /work --entrypoint "$2" "$TAG" web create_agent handler_module=web_rest.tools id=rest ingress_rule=allow_all >/dev/null 2>&1
}

# Serve the head page the ONE gated way — `/head/file/index.html` via read_stream,
# no FANTASTIC_WEB_INDEX env. The file_bridge clamps every root INSIDE the running
# dir (=/work), so the baked head is first COPIED into the workdir, then served by
# a read-only file_bridge rooted at the relative `head`. $1=workdir $2=bin $3=root
compose_head() {
  $ENGINE run --rm -v "$1:/work" --entrypoint sh "$TAG" -c 'mkdir -p /work/head && cp /opt/fantastic/head/index.html /work/head/index.html' >/dev/null 2>&1
  $ENGINE run --rm -v "$1:/work" -w /work --entrypoint "$2" "$TAG" "$3" create_agent handler_module=file_bridge.tools id=head root=head readonly=true ingress_rule=allow_all >/dev/null 2>&1
}

# ── build ──────────────────────────────────────────────────────────────────
if [ "$BUILD" = 1 ]; then
  echo "== build =="
  sh "$REPO/container/build.sh"
fi

# ── runtime-field check (one-shot reflect, no daemon) ───────────────────────
# All three runtimes are full HTTP servers (swift's web is swift-nio now), so
# each appears in the one-shot reflect check AND the serve checks below.
echo "== reflect.runtime per runtime =="
for pair in "python:/opt/fantastic/venv/bin/fantastic_kernel" \
            "rust:/opt/fantastic/bin/fantastic-rust" \
            "swift:/opt/fantastic/bin/fantastic-swift"; do
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
for rt in python rust swift; do
  P=$(freeport); tmp=$(mktemp -d); c="ftsmoke-$rt-$$"
  CONTAINERS="$CONTAINERS $c"
  case "$rt" in
    python) bin=$PYBIN; root=kernel_state ;;
    rust) bin=$RUSTBIN; root=core ;;
    swift) bin=$SWIFTBIN; root=core ;;
  esac
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
compose_web "$tmp" "$PYBIN" kernel_state "$P"
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

# ── `/` is the agent-tree index (both runtimes); the head rides the gated route ─
# No FANTASTIC_WEB_INDEX env: `/` is ALWAYS the live agent-tree index. The
# all-readmes head page is served the one gated way — a read-only file_bridge
# over /opt/fantastic/head, reached at /head/file/index.html (read_stream).
echo "== / is the agent index; head served via /head/file/index.html (gated) =="
for spec in "python:$PYBIN:kernel_state:" "rust:$RUSTBIN:core:-e FANTASTIC_RUNTIME=rust"; do
  rt=${spec%%:*}; rest=${spec#*:}; bin=${rest%%:*}; rest=${rest#*:}; root=${rest%%:*}; envflag=${rest#*:}
  P=$(freeport); tmp=$(mktemp -d); c="ftsmoke-head-$rt-$$"
  CONTAINERS="$CONTAINERS $c"
  compose_web  "$tmp" "$bin" "$root" 8088
  compose_head "$tmp" "$bin" "$root"
  # shellcheck disable=SC2086
  $ENGINE run -d --name "$c" -p "127.0.0.1:$P:8088" -v "$tmp:/work" $envflag "$TAG" >/dev/null
  up=""; i=0; while [ $i -lt 100 ]; do curl -sf -o /dev/null "http://127.0.0.1:$P/" && { up=1; break; }; sleep 0.5; i=$((i+1)); done
  page=$(curl -s "http://127.0.0.1:$P/" 2>/dev/null || true)
  # `/` must be the agent index (NOT the head page).
  if [ -n "$up" ] && ! printf '%s' "$page" | grep -q "KERNEL HEAD"; then
    ok "$rt: / serves the plain agent-tree index (no head back-channel)"
  else
    bad "$rt: / unexpectedly served the head page or never came up ($($ENGINE logs "$c" 2>&1 | tail -2 | tr '\n' '|'))"
  fi
  # The head page is reachable through the gated file_bridge route.
  hp=$(curl -s "http://127.0.0.1:$P/head/file/index.html" 2>/dev/null || true)
  if printf '%s' "$hp" | grep -q "KERNEL HEAD" && printf '%s' "$hp" | grep -q "Aisixteen Fantastic"; then
    ok "$rt: head page served via /head/file/index.html (read_stream, gated)"
  else
    bad "$rt: head not served through the file_bridge route ($($ENGINE logs "$c" 2>&1 | tail -2 | tr '\n' '|'))"
  fi
  $ENGINE rm -f "$c" >/dev/null 2>&1 || true; rm -rf "$tmp"
done

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
  ok "no web agent autocreated in the blank workdir"
fi
$ENGINE rm -f "$c" >/dev/null 2>&1 || true; rm -rf "$tmp"

echo "== result: $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
