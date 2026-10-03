# `configs/` — kernel configs, and `../specs/` — what is asserted about them

**Two files per scenario, deliberately separated.**

| file | declares | may contain |
|---|---|---|
| `configs/<id>.yaml` | the SYSTEM SHAPE — `loaders`, each with its manifest roster | nothing test-specific; it is deployable as written |
| `specs/<id>.yaml` | what this suite ASSERTS — `config:`, `name`, `description`, `requirements`, `expect` | no system declarations |

A config that carried `requirements:` or `expect:` would not be the thing you would
ship, so the thing under test would stop being the thing that runs in production. The
parser is strict in BOTH directions: an unknown key in either file raises, naming the
file it was found in, so a test key drifting into a config surfaces immediately.

Discovery globs `specs/*.yaml` — the suite is enumerated by what asserts, and each spec
names its config. Adding a scenario is adding two files; the runner is never edited.

A config declares a kernel shape and the verdicts that shape must produce. The
engine boots exactly it and the general runner reconciles the RUNNING system
against it. Discovery is a glob (`configs/*.yaml`): adding a scenario is adding
a file, never editing the runner.

The layout is borrowed from openfga's `sample-stores` (`store.fga.yaml`: one
file per subject, the declaration and its assertions side by side) and the
requirement-ID discipline from `a2a-tck` (stable ids, duplicate detection at
build time, MUST→assert / SHOULD→xfail). Neither of them reconciles a *declared
shape against a running system* — they assert behaviour with the declarative
file as input. That step is ours; this file is where it hangs.

---

## Grammar

Parsing is **strict**: an unknown key anywhere raises `ConfigError`. That is a
deliberate inversion of `static_loader`, which drops every top-level key but
`agents:` in silence and never objects to a misspelled meta (`can_calls:`
becomes inert record meta with no complaint on any channel). A contract file
that quietly ignores half of what you wrote is the exact failure this suite
exists to catch.

```yaml
name: roster_shape                 # defaults to the file stem
description: >                     # what this scenario is for
  ...
requirements: [C20, C23]           # ids from _engine.REQUIREMENTS; a typo raises at load
                                   # SCENARIO ids only — PC-1…PC-5 run on every
                                   # config and naming one raises (see below)

loaders:                           # one or more static_loader agents
  - id: static_loader              # REQUIRED — it is the loader's agent id
    file: fantastic.yaml           # the manifest's name in the test cwd; defaults to <id>.yaml
    meta: {display_name: roster}   # extra meta for the loader's OWN create_agent
    manifest:                      # ← exactly ONE of these three:
      agents:
        - id: memory
          handler_module: file_bridge.tools
          allow: ['.']
          ingress_rule: allow_all_ingress
          callable_by: {worker: [write, read]}
    # manifest_text: "agents: [ this is : not : valid"   # verbatim bytes (malformed-yaml cases)
    # missing_manifest: true                             # nothing written — the path is not there

expect:
  hydration: ok                    # ok | refused_at_write | boot_error
  error_contains: "cannot read manifest"   # REQUIRED unless hydration is ok — pin the message
  reboot: true                     # default true; PC-4 skips when false
  closed_roster_sender: worker     # who probes PC-3; defaults to the first roster id
  edges:                           # reachability probes — see below
    - name: the declared worker→memory edge is usable
      from: worker
      to: memory
      send: {type: set, key: probe, value: 1}
      allow: true
    - from: worker
      to: quiet
      send: {type: list, path: ""}
      allow: false
      side: ingress                # only meaningful on a denial
```

**Placeholders.** `$root` — a whole string, key or value, anywhere — is THIS
runtime's root id. `$workdir` — a string VALUE in a loader's `manifest` that is
`$workdir` or starts with `$workdir/` — is the kernel's running dir, ABSOLUTE (the
test's own tmp dir), substituted when the runner writes the manifest; keys and
`manifest_text` are written verbatim. It names an absolute path that is still the
test's: `file: $workdir/creds.yaml` (a credential_store refuses a relative `file`,
and a test never touches the user's home).

## `requirements:` names what THIS file adds

The five core ids (`PC-1`…`PC-5`) are pinned by `test_manifest_contract.py` on
every config in this directory, by construction — shape, edges, closure, reboot,
verdict. A config that listed one would be claiming credit for a walk it does
not control, so `load_config` **raises** on a core id. What belongs here is the
scenario's own ids: the ones its validator beside the runner tags with
`check(...)`. Engine fixtures declare none — the runner IS their subject.

## `edges:` are probes, not grant maps

**Assert reachability, never a config field.** A field check stops tracking the
property the moment a second gate exists. Measured on `static_loader`'s own
fixture: the manifest granted `worker.can_call = {memory: [set, read]}`, the
field was set, and the send came back `{"reason": "unauthorized", "side":
"ingress"}` — the callee never named the caller back. The reverse case is just
as real: an agent carrying `ingress_rule=allow_all_ingress` is still unreachable
with no `callable_by`.

So an edge is an ATTEMPT. The runner sends `send:` from `from:` to `to:` through
`reach()` (an agent-origin send — a bare `kernel.send` is an *operator* send and
passes ungated, proving nothing) and asserts what the caller actually got:

- `allow: true` → `reason != "unauthorized"`. Asserted as a NEGATIVE: a pass
  returns the handler's own reply, which may itself carry an `error` (a sealed
  leg, a missing store). That is a different animal from a refusal.
- `allow: false` → `reason == "unauthorized"`, a non-empty `hint`, and — when
  declared — `side` must match. `side` is the evidence the gate is two-sided at
  all: `egress` means the send never left the sender, `ingress` means the callee
  refused it, `mutator` means a tree mutator named an operand that is not the
  sender's own child.

## The three hydration verdicts

`static_loader` refuses on two entirely different channels, and only one of them
is loud by default. A config must name which it expects.

| `expect.hydration` | what happened | where it surfaces |
|---|---|---|
| `ok` | the roster built | the create reply carries a record; the explicit `boot` says `booted: true` |
| `refused_at_write` | the `manifest` field has the wrong SHAPE (not a non-empty path string) — the only write-time check | `validate_meta` → `{"error": …, "reason": "invalid_meta"}`, and the loader never enters the roster |
| `boot_error` | the manifest could not be READ (missing, or escaping the running-dir clamp), or it read fine and an ENTRY failed | **nowhere, by default** — the create reply looks clean |

That last row is why the engine always re-sends `boot`. `_verb_create_agent`
boots the child it just made, DISCARDS the reply and swallows the exception
(`kernel/_agent.py:1231-1236`), while `static_loader._boot` never raises — it
RETURNS `{"error": …, "booted": false}` and returns on the FIRST bad entry. A
manifest with a bad entry therefore produces a clean-looking create, a silently
PARTIAL roster (entries before the bad one exist and persist, entries after it
never do) and no complaint on any channel. Without the explicit re-boot the
suite would greenlight a half-built kernel.

## Known gaps — what the manifest cannot express

The rule is: **do not extend `static_loader`**. There is no layering step — a
config declares loaders and nothing else, so anything the manifest cannot say is
a gap to be fixed at its cause, not worked around per scenario. What would have
to change is noted per gap.

1. ~~**The persistence store cannot be declared.**~~ **FIXED.** `_find_store`
   (`kernel_state/tools.py`) scanned only the DIRECT CHILDREN of the root loader,
   so a store declared in a manifest landed a grandchild and was never
   discovered — `aflush` then no-opped with no error anywhere and the tree
   silently stayed in RAM. Declaring it was never the problem; finding it was.
   It walks the subtree breadth-first now (shallowest match still wins), so the
   store is an ordinary roster entry. This is what removed `prelude:`.

2. **Nesting cannot be declared.** Every entry is created as a child of the
   loader (`static_loader/tools.py:106`). A `parent:` key crashes `Agent.create`
   with `TypeError: got multiple values for keyword argument 'parent'` — inside
   the swallowed boot, so the loader boots inert and the create reply still
   looks clean. A `parent_id:` key is worse: it is not stripped, so the RECORD
   reports a parent the tree does not have, `$parent` resolves asymmetrically
   between the two gate halves, and a reboot silently overwrites the lie from
   disk position. *Would need:* a real parent key in the spec, stripped and
   honoured by `create_agent` (the loader passes its own id as `parent_id`).
   No scenario needs deeper nesting today.

3. **An id-less entry is not idempotent.** Skip-by-id
   (`static_loader/tools.py:100-102`) is the ENTIRE idempotency mechanism, so an
   entry with no `id` mints a fresh `<bundle>_<hex6>` on every boot — including
   the hydrator's own explicit re-boot, which is why a single hydration of such
   a config already produces two. PC-1 refuses the entry and PC-4 catches the
   duplicate. Explicit ids are not a style preference.

4. **The skip is GLOBAL, not scoped to the loader.** An id already taken
   anywhere (an ephemeral `cli`, another loader's roster) is silently `skipped`,
   so a declared `handler_module` can quietly never take effect. PC-1's reverse
   direction is what catches it.

5. **No ordering, no dependency, no top-level metadata.** Entries create
   top-to-bottom; a `can_call` naming a later id is not
   validated at all; and nothing in a `fantastic.yaml` describes what the built
   system SHOULD look like. That last one is why this wrapper format exists.

## Files here

`engine_smoke*.yaml` are ENGINE fixtures, not scenarios: they exist so
`hydrate` / `dehydrate` / `reach` and all five runner invariants are exercised
independently of any scenario, and they cover two of the three hydration
verdicts between them (`ok`, `boot_error` — an unreadable manifest and a bad
entry); no shipped fixture declares `refused_at_write`.

Every other config is a SCENARIO, run by the one general runner
(`src/lib/python/tests/agentic/test_agentic.py`, and each port's corpus runner)
against its spec in `../specs/`.

| config | pins |
|---|---|
| `two_sided_gate.yaml` | TSG-1…TSG-6 — an edge is the intersection of both halves, and a denial names the half that refused |
| `sealed_by_default.yaml` | C23, C20 — an undeclared capability errors; the io seal is not the call gate |
| `owned_subtree_delete.yaml` | C17, C18 — a delete consumes one loader's roster and spares its neighbour; a refused one mutates nothing |
| `lifecycle.yaml` | C14…C16 — cancel stops the work, terminal states absorb, four failure classes stay four shapes |
| `namespacing.yaml` | C20 — a child's state belongs to its POSITION in the tree |
| `exec_shell.yaml` | EXS-1…EXS-5 — a PTY `start` with no `command` runs the child's own `$SHELL` (autodiscovered from `inherit_env`/`env`, no fallback); the record's `shell` permits exactly `$SHELL` / `$SHELL -l` |
| `credential_store.yaml` | CRED-1…CRED-5 — `set` stores a declared ref's value and `get` answers it; the call gate alone decides who reaches a ref; the host is the declaration's (`set` takes none) and a pinned host must match; forget drops the value and keeps the declaration, and `set` never declares a ref |

`lifecycle.yaml` names one non-shipped bundle, `lifecycle_probe.tools`, which
lives at `tests/agentic/lifecycle_probe/` and is registered UNLISTED by the
suite's conftest (python resolves a `handler_module` only from the bundle catalog
plus `register_unlisted`, as rust and swift resolve only what they registered),
because the manifest's `handler_module` has to resolve inside `Kernel.load` at
reboot as well as at dispatch.

### What a validator may assert

Not the five core invariants — the runner walks those for every config here, and
a validator that repeated one would be two truths about one file. A validator
asserts what its scenario is FOR: an effect (did the byte land?), a value (which
document did the read see?), a causal sequence (open one half, then the other), a
sweep (is anything reachable that nobody declared?). Where a config declares a
probe as an `edges:` entry, PC-2 owns its shape — `denied`, the `side`, the
`hint` — and the validator picks up where that stops.
