# agentic_advanced — where the others fail, measured

The canonical suite (`tests/conformance/`, `tests/kernel/`) proves parity with
the industry's own conformance assertions. THIS suite is the other half: tests
built from the competition's **documented failures** (web sweep, Aug 2026) that
their architectures cannot pass and ours passes structurally — with zero kernel
changes. Every claim here is a measured green test, not positioning prose.

## The five advancements

| test | their failure (sourced) | why we pass structurally |
|---|---|---|
| `test_no_double_fire.py` | LangGraph re-runs an interrupted node FROM THE TOP on resume: side effects before `interrupt()` fire twice (maintainer issue #6208: "a node with two interrupts will rerun after only one resume"; blog.raed.dev/posts/langgraph-hitl — duplicate tickets, invisible in history). Their fix: "make your side effects idempotent" — developer discipline. | Suspension is a message that ended, not a checkpoint. Resume is a NEW message. The test's resume path DELIBERATELY replays every recorded action id (the wrong implementation, LangGraph's actual behavior) — and nothing re-fires, because action ids are ledger executions: a replay JOINS the recorded outcome. Idempotency is the substrate's property. |
| `test_drift_loud_resume.py` | Durable replay silently continues on a world that moved — recorded tool calls against drifted/renamed tools, stale facts ride forward ("The Hidden Replay Risk in LangGraph", Medium 2026). Nothing revalidates a recording. | A recorded run is data; re-firing is a LIVE send — gate + roster revalidate every hop. Measured and pinned: **the gate outranks the ledger** — after revocation even a COMPLETED execution's id refuses instead of joining (live authority beats history); a deleted target names itself; a renamed verb dies at the verb-scoped egress before the handler is reached. |
| `test_midrun_narrowing.py` | Octomind ripped LangGraph out: no way to inspect or modify an agent's tool set mid-run based on what it discovered — the graph is compiled before the run. | Authority is table rows, not compiled topology. `update_agent` rewrites a RUNNING agent's `can_call` between two of its steps; the very next call refuses, and the typed refusal lands INSIDE the agent's own message where it can react. No restart, no recompile. |
| `test_provenance_laundering.py` | `Command(resume=v)` is an untyped value with no provenance — if the resume channel is LLM-reachable, the AI approves itself. General class: ambient authority — any hop that reaches the runtime holds the runtime's full power. | `sender=None` (operator) exists ONLY in-process. A wire frame dispatches AS ITS LEG AGENT, a mutator needs both halves of the leg→root edge and then acts on the leg's OWN children only. The same create_agent envelope: refused over the wire (refusal NAMES the leg as sender), runs freely in-proc as operator. |
| `test_hostile_sidecar.py` | LangGraph's persistence layer WAS the attack surface: SQL injection in the SQLite checkpointer chained to RCE (patched June 2026 — The Hacker News / CSA). The store executed attacker-shaped strings. | Messages live in a MEMORY STORE agent (`ram_store`: tables in RAM, no SQL, no eval); the test `inject`s broken and hostile tables — a duplicate id (`exists`) or a dangling fold ref (`bad_arg`) is REFUSED loud and changes nothing; well-formed hostile rows (SQLi-shaped text, a `__import__('os').system` payload) come back as data through the ledger and named-call dedup, and nothing executes. |

## What we deliberately did NOT build here (and why)

- **Cross-session wall (session sub-kernels).** Two session sub-kernels under
  one manager; A cannot address B; shared singletons reachable by both. The
  differentiator vs `thread_id`-by-convention is real, but the py sub-kernel
  fixture rides the kernel-switcher plane (contract 24/25/26) that is not
  landed py-side yet. Build it there, not as a one-off fixture here.
- **Denials as an audit signal.** "Track permission denials as compromise
  indicators" is a stated 2026 market want. Today a gate refusal settles
  `pid:null, status:failed` and is NOT ledgered — an agent probing walls
  leaves no queryable trace. Whether refusals belong in the ledger is a
  kernel decision (id space, ring pressure, origin scoping) — parked for the
  operator, not smuggled in via a test.
- **Content-binding (approve X, execute Y).** Decided client scope: what the
  human approves is what the client renders vs the proposal — client honesty.
  The execution id still binds the click to one target and one run.
- **Payload injection / semantic correctness.** Nobody solves these; we don't
  claim them. See `tmp/agentic_development.md` §8 escape-class table.

## Relation to the canonical suite

`tests/conformance/` + `tests/kernel/` = "we meet the bar they set for
themselves" (their exemplar tests, re-keyed to our substrate). This folder =
"we pass what they measurably fail." Both suites run offline, scripted, zero
LLM cost, and are part of the normal pytest sweep. The live/paid mirrors of
these claims live in `tmp/live_*.py` probes and `e2e/`.

Sources (Aug 2026): github.com/langchain-ai/langgraph issue #6208 ·
blog.raed.dev/posts/langgraph-hitl · medium.com/@mehul_parmar (hidden replay
risk) · thehackernews.com/2026/06/langgraph-flaw-chain-exposes-self.html ·
octomind.dev postmortem (via LG-limitations roundups) ·
northflank.com/blog/how-to-sandbox-ai-agents (denial-tracking market want).
