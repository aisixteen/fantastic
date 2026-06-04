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
  [ -d "$tmp/.fantastic" ] && ok "$rt wrote /work/.fantastic (workdir bind ok)" \
    || bad "$rt did not write /work/.fantastic"
  # SIGTERM-clean: stop should return promptly (tini forwards; no 10s kill wait).
  t0=$(date +%s); $ENGINE stop -t 8 "$c" >/dev/null 2>&1 || true; t1=$(date +%s)
  [ $((t1 - t0)) -lt 8 ] && ok "$rt stops promptly on SIGTERM ($((t1-t0))s)" \
    || bad "$rt did not stop within grace (SIGTERM not handled?)"
  $ENGINE rm -f "$c" >/dev/null 2>&1 || true; rm -rf "$tmp"
done

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

# ── head: the all-readmes page at / + still a live kernel ───────────────────
echo "== head (descriptive all-readmes page on :80, mapped from :8080) =="
P=$(freeport); tmp=$(mktemp -d); c="ftsmoke-head-$$"
CONTAINERS="$CONTAINERS $c"
$ENGINE run -d --name "$c" -p "127.0.0.1:$P:8080" -v "$tmp:/work" \
  -e FANTASTIC_RUNTIME=head "$TAG" >/dev/null
up=""; i=0; while [ $i -lt 100 ]; do curl -sf -o /dev/null "http://127.0.0.1:$P/" && { up=1; break; }; sleep 0.5; i=$((i+1)); done
page=$(curl -s "http://127.0.0.1:$P/" 2>/dev/null || true)
if [ -n "$up" ] && printf '%s' "$page" | grep -q "KERNEL HEAD" \
   && printf '%s' "$page" | grep -q "Aisixteen Fantastic"; then
  ok "head serves the all-readmes page at / (host :$P → :8080)"
else
  bad "head page missing/incomplete ($($ENGINE logs "$c" 2>&1 | tail -2 | tr '\n' '|'))"
fi
hr=$(curl -s -X POST -H 'Content-Type: application/json' "http://127.0.0.1:$P/rest/kernel" -d '{"type":"reflect"}' 2>/dev/null || true)
printf '%s' "$hr" | grep -Eq '"runtime"[[:space:]]*:[[:space:]]*"python"' \
  && ok "head is also a live kernel (reflect over REST → python)" \
  || bad "head reflect-over-REST failed"
$ENGINE rm -f "$c" >/dev/null 2>&1 || true; rm -rf "$tmp"

echo "== result: $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
