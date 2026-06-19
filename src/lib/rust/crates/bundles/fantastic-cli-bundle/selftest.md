# fantastic-cli-bundle selftest

> scopes: cli, rendering
> requires: tty, `cargo build --release --bin fantastic`
> out-of-scope: persistence (ephemeral bundle)

Stdout renderer. Composed per-process when stdin is a tty, never
persisted. Renders `token` / `done` / `say` / `error` events that
travel through the substrate.

## Pre-flight

```bash
rm -rf /tmp/fcli_test
mkdir -p /tmp/fcli_test
cd /tmp/fcli_test
FANTASTIC=/path/to/rust/target/release/fantastic
```

## Tests

### Test 1: tty boot composes cli renderer

Run interactively (stdin is a tty):

```bash
echo 'reflect cli' | $FANTASTIC
```

Expect: a `cli` agent appears in the reflect output, marked
ephemeral, child of `core`. NOT persisted to `.fantastic/agents/`.

### Test 2: non-tty boot does NOT compose cli

```bash
$FANTASTIC reflect </dev/null | jq -e '[.tree | recurse(.children[]?) | select(.handler_module == "cli.tools")] | length == 0'
```

Expect: no cli agent in the tree when stdin is not a tty.

### Test 3: token event prints to stdout

(Requires a backend bundle that emits tokens — wire up against the
forthcoming fantastic-ai bundles when they land.)

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. tty boot composes cli |  |  |
| 2. non-tty omits cli |  |  |
| 3. token event renders | skip | needs ai bundle |
