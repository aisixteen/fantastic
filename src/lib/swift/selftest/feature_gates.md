# feature_gates selftest (Swift)

> scopes: build, packaging
> requires: `swift` toolchain; Xcode with the iPhoneSimulator SDK
> (`xcrun --sdk iphonesimulator --show-sdk-path`)

Verifies the Embedded vs Full tier split. The Full (macOS) tier links
the four subprocess-using bundles; the Embedded tier (iOS, iPadOS,
visionOS, tvOS, watchOS, sandboxed macOS) compile-time **excludes**
them via `#if os(macOS)` gates — in `defaultBundleRegistry()` (so they
never `register(...)`) AND inside each bundle's own source (so it
compiles to empty off macOS):

| handler_module | tier |
|---|---|
| `terminal_backend.tools` | Full only |
| `python_runtime.tools` | Full only |
| `local_runner.tools` | Full only |
| `ssh_runner.tools` | Full only |

The `.build/debug/fantastic` binary IS the Full tier. The Embedded
slice is verified by cross-compiling to a non-macOS triple — the same
`#if os(macOS)` gates that make the bundles disappear from
`defaultBundleRegistry()` make their source compile to empty.

## Pre-flight

```bash
cd /Users/oleksandr/Projects/fantastic_canvas/swift
swift --version
SIM_SDK=$(xcrun --sdk iphonesimulator --show-sdk-path)
echo "$SIM_SDK"   # must print an iPhoneSimulator*.sdk path
```

## Tests

### Test 1: Full tier registers all four subprocess bundles

The default (macOS) build links them; a `create_agent` against each
returns a real agent record (not a weak-load shell).

```bash
BIN=/Users/oleksandr/Projects/fantastic_canvas/swift/.build/debug/fantastic
rm -rf /tmp/fa_full && mkdir -p /tmp/fa_full && cd /tmp/fa_full
for hm in terminal_backend python_runtime local_runner ssh_runner; do
  ID=$("$BIN" core create_agent handler_module=$hm.tools \
    | python3 -c "import json,sys;print(json.load(sys.stdin).get('id','?'))")
  KIND=$("$BIN" "$ID" reflect \
    | python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('kind') or d.get('error'))")
  echo "$hm.tools → $ID kind=$KIND"
done
```
Expected: each line prints a real id and a `kind` (e.g.
`terminal_backend`), NOT `no bundle for handler_module "...tools"`.
Regression signal: a `no bundle` error means a Full-tier bundle fell
out of `defaultBundleRegistry()` on macOS.

### Test 2: Embedded slice cross-compiles without the subprocess bundles

Cross-compile the Embedded umbrella to the iOS Simulator triple. The
`#if os(macOS)` gates exclude all four subprocess bundles' source and
their registry lines; the build must succeed and pull none of them.

```bash
cd /Users/oleksandr/Projects/fantastic_canvas/swift
SIM_SDK=$(xcrun --sdk iphonesimulator --show-sdk-path)
swift build --target FantasticKernelEmbedded \
  -Xswiftc -sdk -Xswiftc "$SIM_SDK" \
  -Xswiftc -target -Xswiftc arm64-apple-ios26.0-simulator
```
Expected: `Build of target: 'FantasticKernelEmbedded' complete!` with
no errors. A benign `using sysroot for 'MacOSX' but targeting 'iPhone'`
warning is expected (SwiftPM resolves the manifest's host sysroot) — it
is NOT a failure. If a future bundle adds a non-gated dep that drags
subprocess code (Process/PTY/SSH) into the Embedded slice, the
cross-build fails here — the iOS Lite tier breaks.

### Test 3: the four handler_modules are absent from the Embedded registry

The Embedded umbrella's dep graph (`FantasticKernelEmbedded` target in
`Package.swift`) must NOT list `FantasticTerminalBackend`,
`FantasticPythonRuntime`, `FantasticLocalRunner`, or
`FantasticSshRunner`; only `FantasticKernelFull` does. This is the
static contract behind the runtime weak-load skip.

```bash
cd /Users/oleksandr/Projects/fantastic_canvas/swift
python3 - <<'PY'
import re
src = open("Package.swift").read()
pro = {"FantasticTerminalBackend","FantasticPythonRuntime",
       "FantasticLocalRunner","FantasticSshRunner"}
def target_deps(name):
    # anchor on the `.target(name: "X", dependencies: [...]` form
    # (not the `.library(...)` product decl that shares the name).
    m = re.search(
        r'\.target\(\s*name:\s*"%s"\s*,\s*dependencies:\s*\[(.*?)\]'
        % re.escape(name), src, re.S)
    return m.group(1) if m else ""
emb = target_deps("FantasticKernelEmbedded")
full = target_deps("FantasticKernelFull")
emb_has = {b for b in pro if b in emb}
full_has = {b for b in pro if b in full}
print("embedded leaks:", emb_has or "none")
print("full has:", full_has)
print("PASS" if not emb_has and full_has == pro else "FAIL")
PY
```
Expected: `embedded leaks: none`, `full has:` all four, and `PASS`.

### Test 4: Full-tier handler_modules weak-load-skip when their bundle isn't registered

Boot a workdir that has a `terminal_backend.tools` record persisted but
run it through a runtime where that bundle isn't registered (the
Embedded contract). The agent SHELL still registers (so children can
wire) but a verb against it returns the no-bundle error — the kernel
keeps booting; it does NOT crash on the missing bundle.

There is no separate Embedded CLI binary (the `fantastic` executable is
macOS-only). Reproduce the weak-load path directly: stage a record for
an unknown handler_module and confirm the shell loads but the verb
fails.

A one-shot RPC (`fantastic <id> <verb>`) loads the disk workdir; the
`reflect` shorthand does NOT (it boots a fresh in-memory kernel), so
use a one-shot RPC here, not `reflect core`.

```bash
BIN=/Users/oleksandr/Projects/fantastic_canvas/swift/.build/debug/fantastic
rm -rf /tmp/fa_weak && mkdir -p /tmp/fa_weak/.fantastic/agents/tb
cat > /tmp/fa_weak/.fantastic/agents/tb/agent.json <<'JSON'
{"id":"tb","handler_module":"made_up_embedded_excluded.tools","parent_id":"core","meta":{}}
JSON
cd /tmp/fa_weak
# Shell loads from disk — `tb` appears in `core list_agents` under core:
"$BIN" core list_agents | python3 -c "
import json,sys
agents = {a['id']: a for a in json.load(sys.stdin)['agents']}
tb = agents.get('tb')
print('tb registered:', tb is not None and tb.get('parent_id') == 'core')
"
# But a verb against it returns the weak-load no-bundle error:
"$BIN" tb reflect | python3 -c "
import json,sys
d = json.load(sys.stdin)
print('PASS' if d.get('error','').startswith('no bundle for handler_module') else f'FAIL {d}')
"
```
Expected: `tb registered: True` (boot did not abort on the unknown
bundle — the shell loaded under `core`) and `PASS` (the verb returns
`no bundle for handler_module "made_up_embedded_excluded.tools"`).
This is the exact path a Full-tier workdir hits on the Embedded tier:
`terminal_backend`/`python_runtime`/`local_runner`/`ssh_runner` records
load as inert shells, verbs against them failfast, the rest of the tree
boots normally.

Note: unlike the Rust kernel, the Swift kernel does NOT print a
`[kernel] skipping agent ...` log line — the weak-load is silent. The
observable signal is the registered-shell-plus-no-bundle-error pair
above, not a log grep.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | Full tier registers all four subprocess bundles | |
| 2 | Embedded slice cross-compiles to iOS Simulator | |
| 3 | four handler_modules absent from Embedded dep graph | |
| 4 | unknown-bundle record weak-load-skips (shell loads, verb fails) | |

## Regression signals

- If Test 2 or Test 3 fails: someone wired a subprocess bundle into the
  Embedded tier (a non-gated dep, or an extra `Package.swift` edge).
  iOS Lite breaks. Fix by gating the source under `#if os(macOS)` and
  keeping the dep off the `FantasticKernelEmbedded` target.
- If Test 4's shell doesn't register OR the verb doesn't failfast: the
  weak-load contract broke. Full-tier workdirs stop loading cleanly on
  the Embedded tier. Fix the kernel's `load(_:)` construct loop.
