# `e2e/` — the live suite

Tests that spend **real model turns**. Nothing here runs automatically and
nothing should: no pytest collection, no CI, no `npm test`. They exist to answer
one question a unit test cannot — *can a model, given only this substrate's own
self-description, actually use it?*

```
e2e/
  run.py                the launcher: picks a backend, runs N tests in parallel
  backends/             THE PLUG — which model, reached how (anthropic|nvidia|ollama|llamacpp)
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

The suite used to carry an `if BACKEND == "anthropic": … elif …` inside each
probe, at three sites each. Thirty branches that drifted independently — and did:
one probe never read `ENDPOINT`, so it died on a 401 while five siblings ran fine
against the same proxy. That class of bug is now unwritable.

The same property makes results comparable. Run `agentic/plan` against a hosted
frontier model and against a local 12B and the only thing that differed is the
plug.

## Groups

| group | what lives there |
|---|---|
| `agentic/` | a model doing real work in the substrate — building, composing, planning, discovering peers, and the kernel contracts (ledger, dead letters, user actions) measured *through* a model |
| `agentic_advanced/` | the hard end: structures that OUTLIVE the conversation — durable sub-agents, wired graphs, A/B on whether naming the graph changes its shape |
| `backends/` | the inference plug itself — each wire measured raw (`wire_anthropic`, `wire_responses`) and the provider contract ai_core relies on, through a live agent (`provider_contract`, any `BACKEND`) |

The top level is by *kind*, deliberately: not every future test type is an LLM
test, and a non-LLM group should not have to pretend to be one.

## Running it

Preflight first — it is free and it names the fix:

```sh
cd src/lib/python && uv run python ../../../e2e/preflight.py
```

Then the launcher:

```sh
cd src/lib/python && \
BACKEND=anthropic \
ENDPOINT=http://127.0.0.1:4000/cc/v1/messages \
FANTASTIC_AI_MODEL=<model the proxy serves> \
ANTHROPIC_API_KEY=<whatever the proxy accepts> \
uv run python ../../../e2e/run.py --lanes 6
```

Against a vLLM server DIRECTLY (no proxy), `BACKEND=vllm` names the server and the
KV-cache table's policy — all required, nothing guessed:

```sh
cd src/lib/python && \
BACKEND=vllm \
VLLM_ENDPOINT=http://127.0.0.1:18000/v1 \
FANTASTIC_AI_MODEL=qwen3.8-27b \
VLLM_KV_SLOTS=7 VLLM_KV_OVERFLOW=evict \
uv run python ../../../e2e/run.py --lanes 1
```

It sends the same repetition knobs as the anthropic plug (`sampling.default.json`) and
leaves the thinking budget to the backend's own per-call rule. A door in front of the
server that wants a key gets it from `VLLM_API_KEY` (sent as a Bearer header only when
set). For the shared GPU the URL is the dataroom deployment's HTTPS door, with its
own key (recipe: `tmp/vllm_backend.md`); keep lanes at 1–2 there — its run slots are
shared with production.

| flag | effect |
|---|---|
| `--lanes N` | how many run at once |
| `--group agentic` | one group |
| `--id plan` | one test (repeatable) |
| `--free` / `--only-free` | include / run only the self-tests, which cost no turns |
| `--list` | show what would run, run nothing |

**Lanes are somebody else's latency.** The GPU behind the proxy holds a fixed
number of sequences and production traffic shares it. `--lanes` is a promise
about that, not a throughput dial.

## No guessed defaults

`BACKEND` and `FANTASTIC_AI_MODEL` are required and never inferred. This is the
same rule the kernel holds itself to, for the same reason: a guessed default is
how a run ends up measuring something other than what you meant, and here that
mistake also costs real turns to discover.

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

Reasoning models think here and that is the intended mode — the anthropic
provider reads **both** channels, wrapping `thinking_delta` blocks in
`<think>…</think>` and streaming them beside the text. Nothing in this suite
turns thinking off.

`think` is not an anthropic field at all. It is a llama.cpp / ollama **record**
field: gemma-4 gates thinking behind `enable_thinking` (its chat template
defaults it false) and emits `<|channel>thought` rather than `<think>` tags. It
is never defaulted here — set `THINK=1` (or `0`) to send it explicitly.

What does need attention is the **budget**. A reasoning model can spend an entire
turn thinking and come back `stop_reason=max_tokens blocks=['thinking']` —
thought present, answer empty. That is a budget problem wearing a model failure's
clothes, so it is a per-test knob: set `limits.max_tokens` in a test's
`test.yaml`, not a global.

## The `/cc` route

The GPU proxy speaks the Anthropic wire at **`/cc/v1/messages`** (raw passthrough
at `/raw`), tunnelled to `127.0.0.1:4000` over SSM.

It returns `tool_use` blocks **for requests that declared no tools** — it parses a
model's NATIVE tool syntax (qwen's JSON `<tool_call>`) server-side. This kernel
declares no tools and reads no JSON from a model: a `tool_use` block in the stream
is a LOUD provider error naming the grammar (`<call name=…>` text), never a
translation. Under the tag grammar the model does not write the syntax the route
parses, so the blocks do not appear; if one does, the model slipped into JSON and the
error says so.

SSM forwards have died mid-run before. If a probe fails on connect rather than on
the substrate, re-check `:4000` before believing the result.
