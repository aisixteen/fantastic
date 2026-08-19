# python_runtime_resolution selftest (Rust)

> scopes: python_runtime, env
> requires: Rust `fantastic_kernel` binary built with `--features full`; a real `python3` you can NAME (one test skips without it)

Exercises the 6-step interpreter resolution ladder of the Rust port.
Python's `_resolve_python` ends at `sys.executable` — the interpreter
already running, a FACT about that process. The Rust kernel is not a python
process, so it has no such fact: it extends the shared tiers with the
`FANTASTIC_PYTHON` env var (an explicit operator choice) and then REFUSES.

It deliberately does NOT probe `which python3` / `which python`. Probing
invents a disk path nobody named, which "by default, no defaults" forbids —
and a job silently running under a discovered interpreter instead of the one
the caller asked for is exactly the failure this ladder exists to prevent.

Two rules the tests below pin, and the difference between them is the whole
design:

- an ABSENT key walks to the next tier — that is PRECEDENCE (Tests 3–5);
- an EXPLICITLY supplied `venv` that holds no interpreter is an ERROR NAMING
  THE PATH (Test 7) — never a walk to a different interpreter. `python` is
  the exception BY DESIGN: it is not existence-checked, because a bare
  command name (`python3.12`) is legal and PATH resolves it at exec.

The ladder is driven through `start` — the bundle's ONLY spawn verb. There is
no `exec`: `start` returns `{job_id, status:"running"}` at once and the program's
output arrives as `progress` events, so each branch is read back with
`status job_id=<id>` (which carries `stdout`). A test that expects `stdout` in
`start`'s own reply is testing a verb that no longer exists.

Every fake interpreter below is a bash script that ignores its script argument
and echoes its branch name — that echo IS the observable, and it lands in the
job's `stdout`.

## Pre-flight

```bash
cd src/lib/rust
cargo build --release --bin fantastic_kernel --features full
BIN=$(pwd)/target/release/fantastic_kernel
WORK=$(mktemp -d)
cd "$WORK"

# A real interpreter for Test 6 — the ladder will not find one for you.
REAL=$(command -v python3 || command -v python || echo "")
[ -n "$REAL" ] || echo "(no real Python — Test 6 skips)"

# Read one job's captured stdout — the ladder's observable.
branch_of() { $BIN py status job_id="$1" | jq -r '.jobs[0].stdout'; }
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
JOB=$($BIN py start code='print("ignored")' python="$PY" | jq -r .job_id)
branch_of "$JOB"
```
Expected: `start` replies `{"job_id": ..., "status": "running"}`, and the job's
`stdout` is `branch1\n` — the override beats every other ladder branch.

### Test 2: payload.venv override (branch 2)

```bash
VENV=$(mktemp -d)
mkdir -p "$VENV/bin"
cat > "$VENV/bin/python" <<'SH'
#!/usr/bin/env bash
echo "branch2"
SH
chmod +x "$VENV/bin/python"

JOB=$($BIN py start code='print("ignored")' venv="$VENV" | jq -r .job_id)
branch_of "$JOB"
```
Expected: `branch2\n`.

### Test 3: record.python (branch 3)

```bash
PY3=$(mktemp -d)/fakepy3
cat > "$PY3" <<'SH'
#!/usr/bin/env bash
echo "branch3"
SH
chmod +x "$PY3"

$BIN core update_agent id=py python="$PY3"
JOB=$($BIN py start code='print("ignored")' | jq -r .job_id)
branch_of "$JOB"
```
Expected: `branch3\n`. The record-level override fires when no
payload-level override is present.

### Test 4: record.venv (branch 4)

```bash
$BIN core update_agent id=py python="" venv="$VENV"
JOB=$($BIN py start code='print("ignored")' | jq -r .job_id)
branch_of "$JOB"
```
Expected: `branch2\n` — the venv-from-record uses the same venv we
built in Test 2. (`python=""` is how a field is UNSET; an empty value
counts as absent, so the walk continues.)

### Test 5: FANTASTIC_PYTHON env (branch 5, Rust-only)

```bash
PYENV=$(mktemp -d)/fakepyenv
cat > "$PYENV" <<'SH'
#!/usr/bin/env bash
echo "branch5"
SH
chmod +x "$PYENV"

$BIN core update_agent id=py python="" venv=""
JOB=$(FANTASTIC_PYTHON="$PYENV" $BIN py start code='print("ignored")' | jq -r .job_id)
branch_of "$JOB"
```
Expected: `branch5\n`. This branch does NOT exist in Python's ladder.

### Test 6: a REAL interpreter, named explicitly

```bash
if [ -n "$REAL" ]; then
    $BIN core update_agent id=py python="$REAL" venv=""
    JOB=$($BIN py start code='import sys; print(sys.executable)' | jq -r .job_id)
    branch_of "$JOB"
else
    echo "skip: no real Python to name"
fi
```
Expected: stdout = `$REAL`'s own `sys.executable`, plus a trailing newline.
This is the first test with a REAL interpreter, so it also proves the prepended
connector imports cleanly. Note the interpreter is NAMED — nothing discovers it.

### Test 7: an explicit venv with no interpreter → refusal naming the path

```bash
$BIN core update_agent id=py python="" venv=""
$BIN py start code='print("nope")' venv=/no/such/venv
$BIN core update_agent id=py venv=/no/such/venv
$BIN py reflect | jq '.python'
```
Expected: `start` replies
`{"error": "python_runtime: payload.venv \"/no/such/venv\" has no interpreter (…)"}` —
no `job_id`, nothing spawned. Then `reflect`'s `python` is
`{"unresolved": "python_runtime: record.venv \"/no/such/venv\" has no interpreter (…)"}`.
Reflect is how a caller discovers its wiring is wrong; it must not report the
interpreter a broken venv would have degraded to.

MUST NOT run the job under some other interpreter. A job that RUNS here is the
bug this test exists for: the caller named a venv for the packages in it, so a
substitute produces `ModuleNotFoundError: <dependency>`, which reads as a broken
dependency rather than a wrong path.

### Test 8: nothing named at all → clean error (branch 6)

```bash
unset FANTASTIC_PYTHON
$BIN core update_agent id=py python="" venv=""
PATH=/tmp $BIN py start code='print("nope")'
```
Expected: `start` itself replies `{"error": "python_runtime: no Python
interpreter resolved — name one: record.python or record.venv …"}` — resolution
happens before the spawn, so there is no `job_id` to chase. MUST NOT panic,
MUST NOT hang. Note this holds with a FULL PATH too: the ladder has no PATH
tier, so a `python3` sitting in `/usr/bin` changes nothing.

## Regression signals

- If any branch falls through unexpectedly (e.g. Test 5 hits the error
  instead of FANTASTIC_PYTHON): the precedence order broke.
  Re-read `resolve_python` in `fantastic-python-runtime/src/lib.rs`.
- If Test 7 spawns a job, or Test 8 succeeds with a `python3` on PATH: a
  fall-through or a PATH probe came back. Both are the same bug — the runtime
  choosing an interpreter the caller never named.
- If Test 8 hangs instead of returning an error: the resolve function
  isn't surfacing the failure to the verb reply. Same file.
- If `start` ever returns `stdout` directly, a blocking run-and-wait verb has
  been reintroduced — that is the retired `exec`, and it is a regression.
