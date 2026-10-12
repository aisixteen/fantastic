# conformance — the canonical suite, built from LangGraph and friends

We mined the test suites of the systems that own this space and re-keyed their
strongest assertions to our substrate. The corpus (cloned to `tmp/repos/`
during the Aug 2026 session): **LangGraph** (checkpointing/time-travel/HITL),
**DBOS** (durable execution — the best single file was
`tests/test_singleexec.py`), **a2a-tck** (the only conformance TCK for an
agent task lifecycle), **openai-agents** (RunState + runtime symmetry),
**langchain checkpoint-conformance** (storage contract), **MCP python-sdk**
(elicitation). Their assertions were distilled into 26 canonical cases
(`tmp/agentic_development.md` §10.5); this folder and `tests/kernel/` close
them. Score: **26/26 accounted — 19 closed with passing tests, 7 N/A by
explicit design decision.**

## What we built, and from whom

| canonical | exemplar it transfers from | our test |
|---|---|---|
| C1 resume-after-suspension continues (question convention, no re-run) | openai-agents `test_run_state.py`, MCP elicitation | `test_c1_suspend_resume.py` |
| C3 approve vs reject diverge, reject model-visible | MCP accept/decline, openai approve/reject | `test_user_actions_topology.py` |
| C4 pending questions resolve by key | LangGraph `{interrupt_id: value}` | PID protocol tests (`tests/kernel/`) |
| C9 completed step never re-executed (incl. across reboot) | DBOS `test_recovery_workflow` | `tests/kernel/test_execution_ledger.py` |
| C10 concurrent same-id runs body once | DBOS `test_singleexec.py` | `tests/kernel/test_execution_ledger.py` |
| C11 idempotency-key dedup | DBOS `test_duplication_policy.py` | `tests/kernel/test_execution_ledger.py` |
| C12 bounded retries → dead-letter | LangGraph RetryPolicy, DBOS DLQ | `tests/kernel/test_execution_ledger.py` (delta: DLQ is a QUERY over terminal rows, not a status) |
| C13 timeout unswallowable | LangGraph/DBOS | `tests/kernel/test_batch_send.py` |
| C14 cancel observable/idempotent | a2a-tck task lifecycle | `tests/kernel/test_task_table.py` |
| C15 terminal state absorbing | a2a `TaskNotCancelableError` | ledger tests (delta: re-send returns the RECORDED OUTCOME, not an error — ensure-semantics) |
| C16–C18 error taxonomy / delete | a2a error range, checkpoint-conformance delete | `tests/kernel/`, `tests/agentic/` |
| C19 paged listing, opaque cursor | checkpoint-conformance `test_list.py` | list_agents/ps paging tests |
| C20 child state namespaced | LangGraph subgraph ns | `tests/agentic/test_namespacing.py` |
| C21 one semantics × N modes | openai runtime-symmetry, a2a ALL_TRANSPORTS | `c21_cases.yaml` + `test_c21_transport_symmetry.py` — STRICTER than the exemplars: 10 data-driven cases byte-diffed across FIVE legs (in-proc / ws / rest / exec-guest / relay); the YAML is the grading contract for the rust/swift ports |
| C22 persisted state survives upgrade | openai run_state corpus | loader version-stamp tests |
| C23 undeclared capability MUST error | a2a `@must` | sealed-by-default suite (`tests/agentic/`) |
| C24 stream order, terminal close, fan-out | a2a `test_multi_stream.py`, DBOS offsets | `test_c24_stream_transport.py` + `test_c24_token_stream.py` — in-proc + wire events + token stream |
| C25 round-trip fidelity incl. bytes/None | checkpoint-conformance `test_put.py` | execution rows flushed to the memory store keep `None`, raw bytes recorded as `{len}` (base64 is banned); `dump` → `inject` round-trips the tables — `tests/kernel/test_memory_store.py::test_stored_rows_keep_none_and_reduce_bytes_to_a_length` |
| forge 1–2 emit-fake, provenance | (no exemplar — ours) | `test_forge_paths.py` |
| forge 3 TOCTOU single-use | (no exemplar — ours) | `test_user_actions_topology.py` (two sockets, same frame id → ONE buy) |

## What we SKIPPED, and why (decisions, not gaps)

- **C2 suspend → serialize → resume in another process** — N/A. The kernel
  does not checkpoint a mind mid-flight; a session is a kernel instance
  (guid + workdir), rehydration is the loader's tree hydration, and the
  dialog is durable per-client history. Nothing to serialize beyond what
  already persists.
- **C5–C8 time travel (replay/fork/re-fire/terminal-noop)** — N/A by
  explicit decision: rewind = git for the recipe + `f(yaml, chat)` replay
  for the run. LangGraph's own C7 semantics (replay RE-FIRES the pending
  question) is the behavior we reject — see the double-execution advancement
  test. Only immutable settled facts serialize here.
- **C26 pending-writes idempotent / reserved channels** — dissolved: there is
  no write-ahead concept; every call commits per-item onto the message tape.
  The reserved-channel idea has no analogue in a message substrate.
- **forge 4 content-binding (approve X, render Y)** — decided CLIENT scope:
  the execution id binds the click to one target and one run; what the client
  renders vs what the proposal holds is client honesty, deliberately outside
  the kernel.

Deliberate deltas from the exemplars (documented divergences, not misses):
no DLQ *status* (dead-letter is a query over the executions), terminal re-send returns the
recorded outcome instead of erroring (ensure-semantics), and stream
subscription is agent-level, not offset-cursor.

## The other half

`tests/agentic_advanced/` is the mirror suite: tests built from what these
same systems **measurably fail** (double execution on resume, silent stale
replay, mid-run rigidity, provenance laundering, checkpointer-as-attack-
surface) — where we pass structurally. Parity here, advancement there.
