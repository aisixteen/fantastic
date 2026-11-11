# `payload_routing_name` (E152) — a routing name in `<payload>` reaches its target (#528)

Free, Rust lane: no model. A scripted vllm plug plays the model of one `ai_chat.tools` chat on the
PACKAGED host; the target is a test-only folder bundle `notekeep` whose verb `create` documents
`title` and `body`, logging what it was given to `calls.jsonl`.

- The routing form (`<routing>` with a label `<title>`, `<payload>` with its own `<title>`): the
  target receives the payload whole, its own title, and the next request carries the reply with it.
- The inline form of the same call: `title` there is routing and the verb documents it, so the call
  is refused loud and the verb does not run (the unit twin is `fantastic-ai-core` `tests.rs`
  `a_routing_name_in_the_payload_of_a_routed_call_is_delivered`).

Run (when the owner runs e2e): `--only-free --id payload_routing_name`.
