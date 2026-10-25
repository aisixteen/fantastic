# `e2e/parity` — the python-parity acceptance harness

Python is the oracle. This harness runs the SAME inputs against python and rust,
records what each answered, and diffs the ports against python byte for
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
| `stubs/` | `openai_chat.py` (the chat wire every `vllm_backend` agent in the families talks to: chains, `CASE=` scripts, a responder, fault shapes), `ollama_chat.py`, `ws_peer.py`, `relay.py` |
| `families/` | `tree`, `store`, `wire`, `cli`, `ctx`, `bundle_verbs`, `kernel_verbs`, `bridges`, `ai_paths`, `providers` |
| `rules.yaml` | the normalisation rules N1–N26 (N13 deleted, its id not reused), each with a `why` and the items it must not hide. `diff.py rulecheck` is their own test — 112 cases, and it must stay all-green |
| `residuals.yaml` | known differences, each with an owner decision and a state |
| `proofs.yaml` | plan item id → the selector or unit twin that proves it |
| `landed.yaml` | which items have landed per runtime — an entry makes a diff line a REGRESSION |

Output goes to `e2e/runs/parity/` (covered by `e2e/.gitignore`).

## Running it

```sh
R=/Users/oleksandr/Projects/fantastic_canvas
PY=$R/src/lib/python/.venv/bin/python

# 1. preflight — fails loud, repairs nothing
$PY $R/e2e/parity/run.py preflight --runtimes python,rust

# 2. only after a fix lands (this is the ONLY thing that builds)
$PY $R/e2e/parity/run.py stage --runtime rust

# 3. the captures. `--slot` (exactly two alphanumerics) names each capture's own
#    work root, `runs/parity/w/<slot>/` — captures with different slots run SIDE BY
#    SIDE, each with its own log. The slot is always two characters, so every path
#    an answer embeds has the same length on every runtime.
#    ONE run directory holds every capture of one acceptance run:
#    <stamp>/{python,python_b,rust}, and the diffs land in <stamp>/diff.
#    The python baseline runs twice — only when e2e/parity/** or src/lib/python changed.
S=$R/e2e/runs/parity/<stamp>
$PY $R/e2e/parity/run.py capture --runtime python --families all --slot p1 --out $S/python   > $S/python.log 2>&1 &
$PY $R/e2e/parity/run.py capture --runtime python --families all --slot p2 --out $S/python_b > $S/python_b.log 2>&1 &
$PY $R/e2e/parity/run.py capture --runtime rust   --families all --slot r1 --out $S/rust     > $S/rust.log 2>&1 &
wait

# 4. the harness's own test: two python runs must agree exactly
$PY $R/e2e/parity/diff.py self --a <stamp>/python --b <stamp>/python_b

# 5. the ports against python
$PY $R/e2e/parity/diff.py port --base <stamp>/python --other <stamp>/rust

# 6. the status line
$PY $R/e2e/parity/diff.py status --diff <stamp>/diff   # diff.py writes there itself
$PY $R/e2e/parity/diff.py check-proofs
```

`--slow` adds A13.18 (45 s of socket idle) and the 2 MB body. `--families` takes
a subset FOR ITERATION ONLY — an acceptance run is always `all`.

`--families all` takes roughly 13 minutes per runtime; `store`, `ai_paths`,
`ctx` and `wire` are the long families.

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

The status line is `rust pass/total`, where an item passes when
it is in `landed.yaml`, its selectors produce no lines, and its unit twins exist
with the §5.9 suites green. "Verified" only at `pass == total`.

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
