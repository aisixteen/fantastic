# `payload_routing_name` (E152) — a routing name in `<payload>` reaches its target (#528)

Free, Rust lane: no model, no fixture. A scripted vllm plug plays the model of one `ai_chat.tools`
chat on the PACKAGED host; the target is the kernel's own `create_agent`, which takes `id` (the new
agent's id), and `id` is also a routing name (the call's label).

- The routing form (`<routing>` with a label `<id>`, `<payload>` with its own `<id>`): the new
  agent exists under the payload's id, and the label is nowhere on its record.
- The inline form of the same call (no `<routing>`, no `<payload>`): the parser refuses it, the
  refusal shows the routing form, and nothing is created. (The unit twins are `fantastic-ai-core`
  `tests.rs` `a_routing_name_in_the_payload_of_a_routed_call_is_delivered` and
  `an_inline_send_is_refused_and_nothing_reaches_the_target`.)

Run (when the owner runs e2e): `--only-free --id payload_routing_name`.
