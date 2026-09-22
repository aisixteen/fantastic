# `e2e/parity` — the python-parity acceptance harness

Python is the oracle. This harness runs the SAME inputs against python, rust and
swift, records what each answered, and diffs the ports against python byte for
byte under a named, reviewable set of normalisation rules.

It replaces the five scratchpad capture scripts and their dozen ad-hoc probes.
Each of those chose its own runtime, its own output format and its own idea of
what counted as a reply; three of them sorted JSON keys, two truncated error
bodies, and one substituted a guessed id list when a step failed. The rules they
each got wrong are one module here.

**It spends nothing.** Every model endpoint a capture reaches is a stub under
`stubs/`. `e2e/backends/connection.local.env` is not read anywhere in this
directory, and no capture reaches a real provider. Run it at every landing point.

## Layout

| path | what |
|---|---|
| `run.py` | `preflight` \| `stage` \| `capture` |
| `spec.py` | the runtime spec, the scrubbed env, the fixed workdirs, the port→role map, `CaptureKernel` |
| `record.py` | every writer; the §5.3 recording rules in one place |
| `diff.py` | `self` \| `port` \| `status` \| `check-proofs` \| `rulecheck` |
| `stubs/` | `anthropic_sse.py`, `openai_chat.py` (nim + llamacpp), `ollama_chat.py`, `ws_peer.py`, `relay.py` |
| `families/` | `tree`, `store`, `wire`, `cli`, `ctx`, `bundle_verbs`, `kernel_verbs`, `bridges`, `ai_paths`, `providers` |
| `rules.yaml` | the normalisation rules N1–N19, each with a `why` and the items it must not hide. `diff.py rulecheck` is their own test — 33 cases, and it must stay all-green |
| `residuals.yaml` | known differences, each with an owner decision and a state |
| `proofs.yaml` | plan item id → the selector or unit twin that proves it |
| `landed.yaml` | which items have landed per runtime — an entry makes a diff line a REGRESSION |

Output goes to `e2e/runs/parity/` (covered by `e2e/.gitignore`).

## Running it

```sh
R=/Users/oleksandr/Projects/fantastic_canvas
PY=$R/src/lib/python/.venv/bin/python

# 1. preflight — fails loud, repairs nothing
$PY $R/e2e/parity/run.py preflight --runtimes python,rust,swift

# 2. only after a fix lands (this is the ONLY thing that builds)
$PY $R/e2e/parity/run.py stage --runtime rust      # or swift

# 3. the python baseline, twice — only when e2e/parity/** or src/lib/python changed.
#    ONE run directory holds every capture of one acceptance run:
#    <stamp>/{python,python_b,rust,swift}, and the diffs land in <stamp>/diff.
$PY $R/e2e/parity/run.py capture --runtime python --families all \
    --out $R/e2e/runs/parity/<stamp>/python
$PY $R/e2e/parity/run.py capture --runtime python --families all \
    --out $R/e2e/runs/parity/<stamp>/python_b

# 4. the ports, one at a time
$PY $R/e2e/parity/run.py capture --runtime rust  --families all --out $R/e2e/runs/parity/<stamp>/rust
$PY $R/e2e/parity/run.py capture --runtime swift --families all --out $R/e2e/runs/parity/<stamp>/swift

# 5. the harness's own test: two python runs must agree exactly
$PY $R/e2e/parity/diff.py self --a <stamp>/python --b <stamp>/python_b

# 6. the ports against python
$PY $R/e2e/parity/diff.py port --base <stamp>/python --other <stamp>/rust
$PY $R/e2e/parity/diff.py port --base <stamp>/python --other <stamp>/swift

# 7. the status line
$PY $R/e2e/parity/diff.py status --diff <stamp>/diff   # diff.py writes there itself
$PY $R/e2e/parity/diff.py check-proofs
```

`--slow` adds A13.18 (45 s of socket idle) and the 2 MB body. `--families` takes
a subset FOR ITERATION ONLY — an acceptance run is always `all`.

Measured durations per runtime (2026-09-19, M1 Pro): tree 22 s, store 145 s,
wire 115 s, cli 80 s, ctx 121 s, bundle_verbs 28 s, kernel_verbs 22 s, bridges
34 s, ai_paths 137 s, providers 72 s — about 13 minutes for `--families all`.

## What makes a runtime CLEAN

1. `preflight` passes.
2. The python self-diff has **0 lines**. Every line there is a harness defect: a
   missing rule (with a `why`), or a step that is not deterministic.
3. Python and the port list the same files. A file present for python and
   missing for a port is a diff line, never a skip.
4. Zero `__harness_error__`, an empty `notes.json`, no quiesce timeouts, and no
   kernel left running under `e2e/runs/parity/work`.
5. Every python↔port diff line maps to an **accepted** residual or to a
   **not-landed** item selector.
6. Zero lines on landed items — one is a regression.
7. Zero unmapped lines. Each is an item nobody wrote down; it belongs in the plan.
8. Zero stale residuals and zero stale selectors.
9. `--check-proofs` passes.

The status line is `rust pass/total · swift pass/total`, where an item passes when
it is in `landed.yaml`, its selectors produce no lines, and its unit twins exist
with the §5.9 suites green. "Verified" only at `pass == total` on both.

## The three rules a new family must not break

**One kernel, one workdir, one recorder.** A family gets `work/<family>` (a real
directory) and `link/<family>` (a symlink beside it), identical strings on every
runtime, wiped before each run. Raw vs resolved paths stay visible — A2.17/A2.20
are exactly that difference — and a count that embeds a path compares exactly,
which a `mkdtemp(prefix=f"pc_{rt}_")` could never do.

**Nothing is invented.** A step that cannot proceed writes `{__harness_error__}`
and the capture exits non-zero. There is no substitute data, no default id list,
no "close enough" reply.

**The harness's own fields are under `harness`.** A timestamp, a wait, a poll
count and a request ordinal are the harness's, not the kernel's; `record.py` puts
them under one key and `diff.py` drops that key by schema, so no per-field
exception list has to be kept in step with the families.

## Known open

* **D12 / `yaml_state`.** `src/lib/python/.venv` carries a stale editable install
  whose module does not import, so python's `bundles=` catalog lists a bundle no
  kernel can load. `preflight` FAILS on it and does not repair it — the choice
  between `uv sync` and pointing the harness at `e2e/dist/python` is the owner's.
  Recorded as residual `R-D12-1`.
* **`landed.yaml` is nearly empty on purpose.** It holds `C12` on both ports,
  which is measured (all three kernels seed `kernel_state`). Filling it in from
  "the suites are green" would turn every open item into a false pass; it is the
  owner's ledger, and the harness holds the runtime to whatever it says.
* **Four items have no proof at all** (`A15.1`, `A15.2`, `A15.3`, `C87`): each
  needs the kernel to fail INSIDE a hook, and no family has that lever yet.
  They are named in `proofs.yaml` under `no_proof` rather than given a selector
  that would pass by matching nothing.
