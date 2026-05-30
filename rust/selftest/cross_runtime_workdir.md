# cross_runtime_workdir selftest

> scopes: persistence, cross-runtime
> requires: Python `uv sync`; Rust binary built with `--features full`

The core promise of the dual-runtime story: the same `.fantastic/`
workdir loads cleanly under either kernel. Records hydrate identically.
Bundles missing in one runtime log + skip without crashing the boot.

This spec proves the round-trip: create a workdir under Python,
open it under Rust, verify state. Then mutate via Rust, switch back
to Python, verify the mutation persisted byte-for-byte.

## Pre-flight

```bash
cd /path/to/fantastic_canvas
PYWORKDIR=$(mktemp -d)
RUSTBIN=$(pwd)/rust/target/release/fantastic

# Build both.
cd rust && cargo build --release --bin fantastic --features full && cd ..
cd python && uv sync && cd ..
```

## Tests

### Test 1: Python writes; Rust reads

```bash
cd "$PYWORKDIR"

# Stage some agents under Python.
fantastic core create_agent handler_module=file.tools id=ff root="$PYWORKDIR" >/dev/null
fantastic core create_agent handler_module=scheduler.tools id=sch file_agent_id=ff >/dev/null
fantastic core create_agent handler_module=python_runtime.tools id=py >/dev/null

# Boot py once so the meta.python auto-fill runs (cf34f47).
fantastic py boot >/dev/null

# Confirm meta.python landed on disk:
grep '"python"' "$PYWORKDIR/.fantastic/agents/py/agent.json"
```
Expected: a line like `"python": "/path/to/python3"` (the absolute
path to Python's `sys.executable`).

### Test 2: Same workdir opened under Rust

```bash
# Same workdir, switch binary.
"$RUSTBIN" reflect 2>&1 | grep -E '"id"|"sentence"'
```
Expected: the reflect output lists agents `core`, `ff`, `sch`, `py`.
NO `[kernel] skipping agent` lines (every Python-shipped bundle has
a Rust equivalent).

### Test 3: Rust uses Python's persisted meta.python

```bash
"$RUSTBIN" py exec code='import sys; print(sys.executable)'
```
Expected: stdout matches the path written in Test 1 BYTE-FOR-BYTE.
Confirms Rust isn't falling back to `which python3` (which might
pick a different interpreter); it reads the deterministic record.

### Test 4: Rust mutates the workdir; Python rehydrates cleanly

```bash
# Schedule a fire via Rust.
"$RUSTBIN" sch schedule target=core payload='{"type":"list_agents"}' interval_seconds=60 >/dev/null

# Verify schedules.json exists + has the schedule.
cat "$PYWORKDIR/.fantastic/agents/sch/schedules.json" | head -5
```
Expected: JSON list containing the new schedule with `interval_seconds:60`.

```bash
# Now boot Python on the same dir and list the schedules.
cd "$PYWORKDIR"
fantastic sch list
```
Expected: the schedule Rust created is visible. Persisted JSON shape
is identical.

### Test 5: Unknown handler_module logs + skips on either runtime

```bash
# Stage a record with a bundle handler that neither runtime ships.
mkdir -p "$PYWORKDIR/.fantastic/agents/ghost"
cat > "$PYWORKDIR/.fantastic/agents/ghost/agent.json" <<'JSON'
{"id":"ghost","handler_module":"made_up_bundle.tools","meta":{}}
JSON

"$RUSTBIN" reflect 2>&1 | grep ghost
cd "$PYWORKDIR" && fantastic reflect 2>&1 | grep ghost
```
Expected: BOTH runtimes emit a line containing
`skipping agent ghost: bundle made_up_bundle.tools not installed in this runtime`
(modulo minor wording — the substring `skipping agent ghost` must
match). Same wire shape for the warn line.

### Test 6: Bundle present in Python but not Rust embedded slice

```bash
# Stage a terminal_backend record (Rust requires --features full).
mkdir -p "$PYWORKDIR/.fantastic/agents/tb"
cat > "$PYWORKDIR/.fantastic/agents/tb/agent.json" <<'JSON'
{"id":"tb","handler_module":"terminal_backend.tools","meta":{"cmd":["bash"]}}
JSON

# Build an embedded-slice CLI (no full feature) and boot the workdir.
cd /path/to/fantastic_canvas/rust
cargo build --release --bin fantastic --no-default-features --features embedded
EMBEDDED_BIN=$(pwd)/target/release/fantastic
cd "$PYWORKDIR"
$EMBEDDED_BIN reflect 2>&1 | grep -E 'tb|skipping'
```
Expected: the embedded binary logs `skipping agent tb: bundle
terminal_backend.tools not installed in this runtime` and continues
booting. iOS Lite would behave the same on this workdir.

## Cleanup

```bash
rm -rf "$PYWORKDIR"
```

## Regression signals

- Test 2 emits a skip line for a Python-shipped bundle: parity broken.
  A bundle is on one side but not the other. Check the bundle scoreboard.
- Test 3 stdout doesn't match Test 1's path: `meta.python` auto-fill
  isn't being read by the Rust resolution ladder. Drift across
  reboots → users get unexpected Python versions.
- Test 5 skip lines diverge in shape: log strings are wire contract.
  AI agents grep them; drift breaks tooling.
- Test 6 crashes instead of skipping: embedded slice doesn't honor
  the weak-load contract. iOS Lite would refuse to boot a workdir
  Pro created — that's the whole point.
