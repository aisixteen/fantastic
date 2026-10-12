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
RUSTBIN=$(pwd)/src/lib/rust/target/release/fantastic_kernel

# Build both.
cd src/lib/rust && cargo build --release --bin fantastic_kernel --features full && cd ..
cd python && uv sync && cd ..
```

## Tests

### Test 1: Python writes; Rust reads

```bash
cd "$PYWORKDIR"

# Stage some agents under Python. The store is the kernel's own `.fantastic`
# (its `allow` must expose the records — deny-all otherwise).
fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools \
    id=ff root=.fantastic allow='["."]' ingress_rule=allow_all_ingress >/dev/null
fantastic kernel create_agent parent_id=kernel_state handler_module=scheduler.tools id=sch >/dev/null
fantastic kernel create_agent parent_id=kernel_state handler_module=exec_bridge.tools id=ex \
    allowed='["echo( .*)?"]' inherit_env='["PATH"]' ingress_rule=allow_all_ingress >/dev/null

# Confirm the command gate landed on disk:
grep -A1 'allowed' "$PYWORKDIR/.fantastic/agents/ex/agent.yaml"
```
Expected: the `allowed` list with `echo( .*)?` — the operator's explicit
choice. There is no default: the record IS the answer.

### Test 2: Same workdir opened under Rust

```bash
# Same workdir, switch binary.
"$RUSTBIN" reflect 2>&1 | grep -E '"id"|"sentence"'
```
Expected: the reflect output lists the root plus `ff`, `sch`, `ex`.
NO `[kernel] skipping agent` lines (every Python-shipped bundle has
a Rust equivalent).

### Test 3: Rust enforces Python's persisted command gate

```bash
JOB=$("$RUSTBIN" ex start command=echo args='["same workdir"]' | jq -r .job_id)
"$RUSTBIN" ex status job_id="$JOB" | jq -r '.jobs[0].stdout'
"$RUSTBIN" ex start command=ls | jq -r .error
```
Expected: the job prints `same workdir`; `ls` is refused naming the
`allowed` patterns — the gate Python wrote is the gate Rust enforces.

### Test 4: Rust mutates the workdir; Python rehydrates cleanly

```bash
# Schedule a fire via Rust.
"$RUSTBIN" sch schedule target=kernel payload='{"type":"list_agents"}' interval_seconds=60 >/dev/null

# Verify schedules.yaml exists + has the schedule.
cat "$PYWORKDIR/.fantastic/agents/sch/schedules.yaml" | head -5
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
cat > "$PYWORKDIR/.fantastic/agents/ghost/agent.yaml" <<'JSON'
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
# The exec_bridge record `ex` from Test 1 (Rust requires --features full).

# Build an embedded-slice CLI (no full feature) and boot the workdir.
cd /path/to/fantastic_canvas/src/lib/rust
cargo build --release --bin fantastic_kernel --no-default-features --features embedded
EMBEDDED_BIN=$(pwd)/target/release/fantastic_kernel
cd "$PYWORKDIR"
$EMBEDDED_BIN reflect 2>&1 | grep -E 'ex|skipping'
```
Expected: the embedded binary logs `skipping agent ex: bundle
exec_bridge.tools not installed in this runtime` and continues
booting. iOS Lite would behave the same on this workdir.

## Cleanup

```bash
rm -rf "$PYWORKDIR"
```

## Regression signals

- Test 2 emits a skip line for a Python-shipped bundle: parity broken.
  A bundle is on one side but not the other. Check the bundle scoreboard.
- Test 3 runs `ls`: the persisted `allowed` gate is not read by Rust —
  the same workdir grants more under one runtime than the other.
- Test 5 skip lines diverge in shape: log strings are wire contract.
  AI agents grep them; drift breaks tooling.
- Test 6 crashes instead of skipping: embedded slice doesn't honor
  the weak-load contract. iOS Lite would refuse to boot a workdir
  Pro created — that's the whole point.
