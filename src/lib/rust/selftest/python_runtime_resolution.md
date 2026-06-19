# python_runtime_resolution selftest (Rust)

> scopes: python_runtime, env, PATH
> requires: Rust `fantastic` binary built with `--features full`; a `python3` on PATH (some tests skip cleanly without it)

Exercises the 8-step interpreter resolution ladder unique to the
Rust port. Python's `_resolve_python` ends at `sys.executable`; Rust
extends with `FANTASTIC_PYTHON` env + `which python3` / `which python`
PATH discovery + an explicit error if nothing resolves.

## Pre-flight

```bash
cd rust
cargo build --release --bin fantastic --features full
BIN=$(pwd)/target/release/fantastic
WORK=$(mktemp -d)
cd "$WORK"

# Confirm a Python is on PATH for the discovery-branch tests; if not,
# the relevant tests must skip with an eprintln (not silently fake).
which python3 || which python || echo "(no Python on PATH — Tests 6/7 skip)"
```

## Tests

### Test 1: payload.python override (branch 1)

```bash
PY=$(mktemp -d)/fakepy
cat > "$PY" <<'SH'
#!/usr/bin/env bash
echo "branch1"
SH
chmod +x "$PY"

$BIN core create_agent handler_module=python_runtime.tools id=py >/dev/null
$BIN py exec code='print("ignored")' python="$PY"
```
Expected: `{"stdout": "branch1\n", ...}` — the override beats every
other ladder branch.

### Test 2: payload.venv override (branch 2)

```bash
VENV=$(mktemp -d)
mkdir -p "$VENV/bin"
cat > "$VENV/bin/python" <<'SH'
#!/usr/bin/env bash
echo "branch2"
SH
chmod +x "$VENV/bin/python"

$BIN py exec code='print("ignored")' venv="$VENV"
```
Expected: `{"stdout": "branch2\n", ...}`.

### Test 3: record.python (branch 3)

```bash
PY3=$(mktemp -d)/fakepy3
cat > "$PY3" <<'SH'
#!/usr/bin/env bash
echo "branch3"
SH
chmod +x "$PY3"

$BIN core update_agent id=py python="$PY3"
$BIN py exec code='print("ignored")'
```
Expected: `{"stdout": "branch3\n", ...}`. The record-level override
fires when no payload-level override is present.

### Test 4: record.venv (branch 4)

```bash
$BIN core update_agent id=py python="" venv="$VENV"
$BIN py exec code='print("ignored")'
```
Expected: `{"stdout": "branch2\n", ...}` — the venv-from-record uses
the same venv we built in Test 2.

### Test 5: FANTASTIC_PYTHON env (branch 5, Rust-only)

```bash
PYENV=$(mktemp -d)/fakepyenv
cat > "$PYENV" <<'SH'
#!/usr/bin/env bash
echo "branch5"
SH
chmod +x "$PYENV"

$BIN core update_agent id=py python="" venv=""
FANTASTIC_PYTHON="$PYENV" $BIN py exec code='print("ignored")'
```
Expected: `{"stdout": "branch5\n", ...}`. This branch does NOT exist
in Python's ladder.

### Test 6: which python3 (branch 6)

```bash
if which python3 >/dev/null 2>&1; then
    unset FANTASTIC_PYTHON
    $BIN py exec code='import sys; print(sys.executable)'
    # Expected: stdout = the absolute path of `which python3`.
else
    echo "skip: no python3 on PATH"
fi
```
Expected: stdout = absolute path of `which python3` output, plus a
trailing newline.

### Test 7: which python fallback (branch 7)

This is hard to test deterministically without a hostile PATH. The
substrate covers it; skip in selftest unless you can synthesize a
PATH with only `python` (no `python3`).

### Test 8: no interpreter resolved → clean error (branch 8)

```bash
unset FANTASTIC_PYTHON
PATH=/tmp $BIN py exec code='print("nope")'
```
Expected: a reply with `{"error": "python_runtime: no Python
interpreter resolved; set record.python or FANTASTIC_PYTHON"}` (or
substantially similar wording). MUST NOT panic, MUST NOT hang.

## Regression signals

- If any branch falls through unexpectedly (e.g. Test 5 hits `which
  python3` instead of FANTASTIC_PYTHON): the precedence order broke.
  Re-read `resolve_python` in `fantastic-python-runtime/src/lib.rs`.
- If Test 8 hangs instead of returning an error: the resolve function
  isn't surfacing the failure to the verb reply. Same file.
