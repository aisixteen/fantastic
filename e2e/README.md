# `e2e/` — the live suite

Tests that spend **real model turns**. Nothing that spends a turn runs
automatically and nothing should: no pytest collection, no `npm test`, and CI runs
only the FREE tests (`costs: free`, no model — `run.py --only-free`, on Linux,
macOS and Windows; see `.github/workflows/ci.yml`). The paid ones exist to answer
one question a unit test cannot — *can a model, given only this substrate's own
self-description, actually use it?*

```
e2e/
  run.py                the launcher: picks a backend, runs N tests in parallel
  backends/             THE PLUG — which model, reached how (ollama|responses|vllm|anthropic)
  helpers/              the shared machinery: bootstrap · proxy reader · AI connector · verdict
  preflight.py          free, instant: would a run fail for a reason that is not the substrate?
  tests/<group>/<id>/   one folder per test
  runs/<stamp>/         per-test logs (gitignored — they carry model output)
```

## One test, one folder

Every test owns its directory, and every directory has the same shape:

| file | what it is |
|---|---|
| `README.md` | what this test proves, and how to run it alone |
| `kernel.yaml` | the agent roster it builds — **checked against `probe.py` before every run** |
| `test.yaml` | limits and knobs; backend-agnostic by construction |
| `probe.py` | the runner |
| `selftest.py` | *(where it exists)* the same fixture driven by a scripted actor, no model |

Adding a test is adding a directory. There is no central list to forget.

## The two axes: what is measured vs. what answers

This separation is the whole design, and it is worth being blunt about why.

**`tests/` never names a model, an endpoint, a key, or a backend.** It says what
to measure and how long to allow. **`backends/` names nothing about a test.** It
says which model, reached how, and what that plug needs before it can run.

There is no `if BACKEND == …: … elif …` inside a probe: branches like that drift
independently (a probe that never reads its endpoint variable dies on a 401 while
its siblings run fine against the same server). That class of bug is unwritable.

The same property makes results comparable. Run `agentic/plan` against a hosted
frontier model and against a local 12B and the only thing that differed is the
plug.

## Groups

| group | what lives there |
|---|---|
| `agentic/` | a model doing real work in the substrate — building a program, planning posture, learning a deep agent's verbs from `docs` |
| `agentic_advanced/` | the hard end: structures that OUTLIVE the conversation — durable sub-agents, wired graphs, A/B on whether naming the graph changes its shape, a master that runs its own worker groups within a bound (`self_managed_groups`, E124) |
| `coding/` | the coder organs, model-driven — file edits, search, git, tests, cancel, job mail, peer review, gated writes — and the context protocol over long runs (folds, recall, restarts, `context_clear`), plus exec_bridge monitors |
| `memory/` | what survives compaction and restarts in the memory store — plans and their reasons, rules, poisoned folds, repeat reads, the budget trigger; the free ones (graceful stop, clean start, rehydrate, growth) need no model |
| `scenario/` | shared scenarios as DATA (`scenario.yaml`), one runner per runtime over `web_rest` — dialog, compose, and the kernel contracts measured *through* a model (ledger, dead letters, user actions); plus scripted runs of the CLI's MCP leg, shell, set-up and canvas |
| `backends/` | the inference plug itself — the Responses wire measured raw (`wire_responses`) and the provider contract ai_core relies on, through a live agent (`provider_contract`, any `BACKEND`) |
| `kernel/` | the kernel's own contracts on every host runtime, NO model (`costs: free`) — reboot continuity, drain and the pid lock, the scheduler across a restart, secret marks, the call gate, exec jobs, the file clamp, folder bundles, kernel-to-kernel bridges and a stub relay, a scripted AI on a mock backend, load (`kernel_load`, solo), the packaged artifact's smoke |
| `connectors/` | an outside service as an agent, NO model — a scripted fake of the service, the kernel's connector on every host runtime over `web_rest`, what crosses the wire (`mcp_client`: tools → verbs, every result mapping, the key on every request) |

The top level is by *kind*, deliberately: not every future test type is an LLM
test, and a non-LLM group should not have to pretend to be one.

## Running it

Preflight first — it is free and it names the fix:

```sh
cd src/lib/python && uv run python ../../../e2e/preflight.py
```

Then the launcher. Against a vLLM server, `BACKEND=vllm` names the server and the
KV-cache table's policy — all required, nothing guessed:

```sh
cd src/lib/python && \
BACKEND=vllm \
VLLM_ENDPOINT=http://127.0.0.1:18000/v1 \
FANTASTIC_AI_MODEL=qwen3.8-27b \
VLLM_KV_SLOTS=7 VLLM_KV_OVERFLOW=evict \
uv run python ../../../e2e/run.py --lanes 1
```

It sends the repetition knobs in `sampling.default.json` as record fields and leaves
the thinking budget to the backend's own per-call rule. A door in front of the
server that wants a key: export `VLLM_API_KEY` and the plug names the `vllm` ref (the
store reads the variable); unset, the record names no `credentials` and no header goes
out. For the shared GPU the URL is the shared deployment's HTTPS door, with its
own key (recipe: `tmp/vllm_backend.md`); keep lanes at 1–2 there — its run slots are
shared with production.

The OpenAI Responses wire is `BACKEND=responses` with `RESPONSES_ENDPOINT` (the full
`…/v1/responses` URL) and `OPENAI_API_KEY`; a local ollama is `BACKEND=ollama`.

### Anthropic (the Messages API)

`BACKEND=anthropic` runs the turns tier against the Anthropic Messages API, with no
GPU. Nothing is guessed: name the full `…/v1/messages` URL, the model, and the key the
store reads:

```sh
cd src/lib/python && \
BACKEND=anthropic \
ANTHROPIC_ENDPOINT=https://api.anthropic.com/v1/messages \
ANTHROPIC_API_KEY=… \
FANTASTIC_AI_MODEL=<the model id> \
ANTHROPIC_CACHING=provider_cache \
ANTHROPIC_PRICE_INPUT_PER_MTOK=<USD per MTok in> \
ANTHROPIC_PRICE_OUTPUT_PER_MTOK=<USD per MTok out> \
E2E_METER=1 \
uv run python ../../../e2e/run.py --group agentic --lanes 1
```

- `ANTHROPIC_CACHING` is `none`, `cleanup` or `provider_cache` (default
  `provider_cache`, so a long run reuses its prompt prefix).
- The two prices are optional, but without them the report has no cost column. Add
  `ANTHROPIC_PRICE_CACHE_READ_PER_MTOK`, `ANTHROPIC_PRICE_CACHE_WRITE_PER_MTOK` and,
  for a model with a long-context tier, `ANTHROPIC_LONG_CONTEXT_THRESHOLD_TOKENS` with
  `ANTHROPIC_PRICE_INPUT_LONG_PER_MTOK` / `ANTHROPIC_PRICE_OUTPUT_LONG_PER_MTOK`.
- `MAX_TOKENS` overrides the output cap (default 32000 in `backends/__init__.py`).
- `E2E_METER=1` writes `meter.jsonl` and `turns/` beside the run's logs; the key is
  forwarded as a header and never written there. `helpers/meter_report.py` prints the
  per-test tokens and dollars.
- The key is read by the `creds` store under the ref `anthropic`, bound to the host of
  `ANTHROPIC_ENDPOINT`; the record names `credentials: {store: creds, ref: anthropic}`
  and never the value.

### Measured cost of one full run

> **PLACEHOLDER — not measured yet.** Fill this in from one paid smoke run on the
> maintainer's key: run one `agentic/` test with `BACKEND=anthropic` and `E2E_METER=1`,
> then read the `usd` column of `helpers/meter_report.py`, and record the model, the
> test, the tokens and the dollars here.

| model | test | requests | prompt tokens | completion tokens | USD |
|---|---|---|---|---|---|
| _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ |

**Keys never reach a backend from the environment.** Every e2e kernel carries a
`creds` credential_store with the `env` backend (`helpers/creds.py`): the key
variable you export is read BY THE STORE, one ref per keyed plug (`responses`,
`vllm`, `anthropic`) bound to that plug's endpoint host, and every keyed AI record names
`credentials: {store: creds, ref: <plug>}`. The root holds a heritable `get` grant
on the store, so an agent a MODEL creates reads its key the same way.

| flag | effect |
|---|---|
| `--lanes N` | how many run at once |
| `--group agentic` | one group |
| `--id plan` | one test (repeatable) |
| `--free` / `--only-free` | include / run only the self-tests, which cost no turns |
| `--list` | show what would run, run nothing |

**Lanes are somebody else's latency.** The GPU behind the endpoint holds a fixed
number of sequences and production traffic shares it. `--lanes` is a promise
about that, not a throughput dial.

## Build or use built

By default a run builds what it tests from source (`E2E_PROFILE`, `debug` unless
set): each kernel through `scripts/build/<runtime>.sh`, the CLI with cargo.
`E2E_ARTIFACTS=<dir>` makes it build nothing and test the binaries in `<dir>`
instead — what `e2e/build.py --out <dir>` writes (the kernels' tarballs with the
probe switches, the `fantastic` CLI a release ships, its PTY rig `screenshot`).
CI builds them once per OS and runs the free suite on exactly those:

```sh
cd src/lib/python
uv run python ../../../e2e/build.py --out /tmp/e2e-art --profile release
E2E_ARTIFACTS=/tmp/e2e-art E2E_PROFILE=release uv run python ../../../e2e/run.py --only-free
```

## No guessed defaults

`BACKEND` and `FANTASTIC_AI_MODEL` are required and never inferred. This is the
same rule the kernel holds itself to, for the same reason: a guessed default is
how a run ends up measuring something other than what you meant, and here that
mistake also costs real turns to discover.

## A model per role

`FANTASTIC_AI_MODEL` is the default model for every AI a probe creates. A probe that
names its roles (`BK.record(role="master")`) lets one run give each role its own
model: `FANTASTIC_AI_MODEL_<ROLE>` (the role upper-cased, any non-alphanumeric
character becoming `_`, so `review-lane` reads `FANTASTIC_AI_MODEL_REVIEW_LANE`)
overrides the default for that role only. Roles are plain strings and none is
hard-coded; a role with no override, an empty override, or a role nobody has heard
of gets the default, and an unset `FANTASTIC_AI_MODEL` is still refused. Every other
record field (endpoint, key reference, caching, output cap) is the same for every
role. A probe prints the resulting role -> model pairs in its log header
(`BK.model_for(role)`, `BK.roles_used()`), so a result says which model played which
part. For `self_managed_groups` (E124, roles `master` and `worker`):

```sh
# one model for every role
BACKEND=anthropic FANTASTIC_AI_MODEL=<model> ...

# one role overridden: the master on its own model, workers on the default
BACKEND=anthropic FANTASTIC_AI_MODEL=<haiku> FANTASTIC_AI_MODEL_MASTER=<sonnet> ...

# both named: a Sonnet master with Haiku workers
BACKEND=anthropic FANTASTIC_AI_MODEL=<haiku> \
  FANTASTIC_AI_MODEL_MASTER=<sonnet> FANTASTIC_AI_MODEL_WORKER=<haiku> ...
```

`peer_review` (E48, roles `coder` and `reviewer`) and `self_managed_groups` (roles
`master` and `worker`) name their roles:

```sh
# a Haiku coder and a Sonnet reviewer
BACKEND=anthropic FANTASTIC_AI_MODEL=<haiku> FANTASTIC_AI_MODEL_REVIEWER=<sonnet> ...
```

The model names are the operator's to give; none is guessed. The rule is unit-tested
without a model in `src/lib/python/tests/e2e_harness/test_model_roles.py`, and the
coder+reviewer wiring in `test_review_lane_roles.py` next to it.

## Reading a result

Probes print a human verdict line *and* a machine one — `E2E-VERDICT: PASS|FAIL`.
The launcher reads only the machine line. A probe that prints neither is reported
`no verdict`, never guessed into a pass: "the probe never said" and "the probe
said no" are different facts, and collapsing them invents a result.

Every run keeps its full transcripts under `runs/<stamp>/`. These cost turns —
re-running to look again is not free.

## The self-tests exist because a fixture lied

`selftest.py` drives a probe's fixture with a *perfect scripted actor* and asserts
it passes. That sounds redundant until it isn't: `subai`'s fixture withheld the
grants its own task required, so the task was unpassable, and every run scored a
model failure that had never happened. The self-test found it in seconds and costs
nothing. Run them whenever a probe starts failing in a way that smells structural.

## Thinking, and its budget

Reasoning models think here and that is the intended mode — the vllm provider
reads **both** channels, wrapping `reasoning_content` deltas in `<think>…</think>`
and streaming them beside the answer. Nothing in this suite turns thinking off.

`think` is an ollama **record** field, never defaulted here.

What does need attention is the **budget**. A reasoning model can spend an entire
turn thinking and come back cut at `max_tokens` — thought present, answer empty.
That is a budget problem wearing a model failure's clothes. `max_tokens` is ONE
constant for every test (`backends.THINK_BUDGET`); `MAX_TOKENS=…` overrides it for
one deliberate run, and vllm_backend keeps its `thinking_token_budget` below it.
