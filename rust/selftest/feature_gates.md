# feature_gates selftest (Rust)

> scopes: build, packaging
> requires: `cargo` toolchain; `rustup` with stable channel

Verifies the `full` vs `embedded` feature gates work as documented —
the iOS embedded slice compiles cleanly without any of the
subprocess-using bundles, and the full build registers all 21
bundles.

## Pre-flight

```bash
cd rust
cargo --version
rustc --version
```

## Tests

### Test 1: workspace compiles under default (full) features

```bash
cargo check --workspace
```
Expected: `Finished ... target(s)` with no errors and no warnings.

### Test 2: CLI compiles with embedded feature only (no full)

```bash
cargo check -p fantastic-cli --no-default-features --features embedded
```
Expected: clean compile. None of the subprocess-using bundles
(terminal_backend, python_runtime, local_runner, ssh_runner) pull
in via path-dep resolution. If a future bundle accidentally adds a
non-`optional` dep to one of those, this check fails.

### Test 3: UniFFI compiles under embedded features

```bash
cargo check -p fantastic-uniffi --no-default-features --features embedded
```
Expected: clean compile. The iOS embedded slice is fundamentally
this build — any new subprocess-using dependency would fail here.

### Test 4: clippy passes with -D warnings under default features

```bash
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: clean. No warnings allowed.

### Test 5: rustfmt parity

```bash
cargo fmt --all -- --check
```
Expected: clean — no diff.

### Test 6: full-tier handler_modules are NOT in the embedded binary

Build the CLI under embedded, boot a workdir that has a
`terminal_backend.tools` record persisted, and confirm the kernel
logs the weak-load skip line.

```bash
WORK=$(mktemp -d)
cargo build -p fantastic-cli --release --no-default-features --features embedded
BIN=./target/release/fantastic

# Stage a record without booting (just write agent.json directly).
mkdir -p "$WORK/.fantastic/agents/tb"
cat > "$WORK/.fantastic/agents/tb/agent.json" <<'JSON'
{
  "id": "tb",
  "handler_module": "terminal_backend.tools",
  "meta": {"cmd": ["bash"]}
}
JSON

# Boot in one-shot mode.
cd "$WORK"
$BIN reflect 2>&1 | head -20
```
Expected: a line matching
`[kernel] skipping agent tb: bundle terminal_backend.tools not installed in this runtime`
in the boot output. The kernel must continue booting (no crash on
missing bundle).

## Regression signals

- If Test 2 or Test 3 fails: someone added a non-optional dep that
  pulls subprocess code into the embedded slice. iOS Lite breaks.
  Fix by either making the dep optional + gating to `full`, or by
  removing it.
- If Test 6's skip line doesn't fire: the weak-load contract broke.
  Workdirs created on Pro stop loading on Lite cleanly. Fix the
  kernel's bootstrap loop.
