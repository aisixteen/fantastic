# `contract/` — what this substrate claims, as DATA

Every assertion in here is runtime-agnostic. A wire-visible property is decidable
from outside, so it is stated ONCE as data and replayed by each runtime's own
runner — instead of being written four times in four languages and drifting.

That is not a theory. The C21 corpus was moved here on 2026-08-27 and its rust
driver found **four real drifts on its first run** — refusals that had lost their
machine-readable `reason` during the port, which nothing had compared before. The
agentic corpus then found **two more, in all three runtimes at once**: the
`locked: true` and `agent is readonly` refusals that #27 declared dropped were
never actually migrated anywhere.

```
contract/
  requirements.yaml        the canonical registry — every id this suite can pin
  conformance/
    cases.json             C21: one envelope, many transports, many runtimes
  agentic/
    configs/*.yaml         DEPLOYABLE kernel shapes — no test metadata, ever
    specs/*.yaml           what each config must prove, as ordered steps
```

## The split: config vs spec

A **config** is a kernel document you could deploy. It describes a system and
says nothing about being tested. A **spec** names the config it runs against and
declares what must be true of it. Nothing test-shaped may leak into a config, and
nothing system-shaped into a spec; both files are parsed strictly, and an unknown
key in either is an error naming the file it was found in.

## The steps DSL

A spec's `expect.steps` is an ORDERED list. Order is the point: a flat table can
state a verdict but never a CAUSE. The two-sided gate is the case that forced it
— opening one grant half at a time and watching the denial move from `egress` to
`ingress` is what proves the second half was refusing independently all along,
invisible behind the first.

| verb | meaning |
|---|---|
| `reach {from,to,send}` | send AS an agent — GATED. The only evidence about reach. |
| `operator {to,send}` | send with operator authority — setup, and side-effect reads. Also the only way to ask about EXISTENCE, since target resolution runs before the gate. |
| `grant`/`revoke {agent,field,to,verbs}` | sugar over an operator `update_agent`. Merges, because `update_agent` REPLACES a field and a bare write would drop the structural baseline. |
| `reboot {}` | persist + rehydrate |
| `assert_tree {same_as: before_reboot, disk: true}` | the dehydrated shape |
| `start` / `await_task` / `collect` | fire a send WITHOUT awaiting it, wait for it in `ps`, then await and assert its Outcome. The cancel rows (C14–C16) need a dispatch that is still running. |
| `reconcile_grants` / `sweep_undeclared` | COMPUTED sweeps — walk what the manifest declares rather than a hand-written list, so a grant somebody added and forgot to assert is still probed |

`expect` matchers: `allow`, `side`, `reason`, `status`, `contains`,
`not_contains`, `error_contains`. Paths in `contains` are dotted with `*` for
"any element": `files.*.name`.

`$name.field` references a value an earlier step bound — a `start` binds its
task, so a later `cancel` can name the pid the kernel minted.

Every step may carry `because: <requirement-id>`, and a spec may not claim a
requirement no step tags. That guard is what keeps the coverage matrix honest.

## What a runtime must implement

Six primitives, and nothing else is per-runtime:

`load_spec` · `hydrate` · `reboot` · `dehydrate` · `reach` · `denied`/`denial_side`

plus the step executor and one probe bundle for the scenarios that need a
cancellable in-flight dispatch. Python's implementation is the reference:
`src/lib/python/tests/agentic/{_engine,_core}.py`.

Two phases run per scenario:

- **CORE** (`_core.py`, PC-1…PC-5) — the price of being a config at all: the
  hydration verdict on the loud channel, the running tree against the declared
  shape *both directions*, a reach to walk, the roster closed after boot, and a
  reboot that rehydrates identically. A spec never lists these.
- **STEPS** — the scenario's own sequence, where cause lives.

Core runs first, reboot included, so the steps execute against a kernel that has
been persisted and rehydrated rather than merely built — and so a scenario that
deletes an agent cannot make PC-4 fail for the right reason at the wrong target.

## Per-runtime exemptions

A runtime that genuinely does not own a verb skips the case BY NAME with the
reason recorded in its driver — never silently. The ts C21 driver does this for
the three tree-CRUD cases: the browser kernel's tree is built by its bundles and
persisted through the host loader, so answering `create_agent` would be
advertising a surface that does not exist. Anything not named must pass.

## The agentic corpus is HOST-RUNTIMES-ONLY (decided 2026-08-27)

Python, rust and swift run `agentic/`. The ts kernel does not, and that is a
decision, not a gap.

Measured: **no scenario applies to it.** Every config builds its roster from
`file_bridge` / `durable_memory` / `exec_bridge`, and the browser kernel has only
view bundles (canvas, html_agent, terminal_view, gl_agent, ai_view, loader). It
also owns no tree-CRUD verbs and no manifest loader — its tree is built by its
bundles and persisted through the HOST loader — so hydrating a declared roster
is not a thing it does. It has no YAML parser either, and ships zero runtime
deps on purpose.

What covers it instead:

- `conformance/cases.json` (C21), which it replays like the others — three cases
  skipped BY NAME, with the reason in the driver.
- its own native tests for the machinery it does have: the two-sided call gate
  (including #39's gated `watch`), the execution ledger, array-only batch send,
  refusals and paging.

Giving it a `file_bridge` so the existing scenarios would run was considered and
rejected: it would grow the browser kernel a filesystem it has no use for
outside tests.

## Root ids differ, deliberately

`$root` in a conformance expectation means THAT RUNTIME'S root — python
`kernel_state`, rust and swift `core`. Parity asserts the per-runtime value,
never equality. The target `kernel` is the alias every runtime resolves to its
own root.
