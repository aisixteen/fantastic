# `payload_routing_name` (E152) — a routing name in `<payload>` reaches its target (#528)

Free, Rust lane: no model, no fixture. A scripted vllm plug plays the model of one `ai_chat.tools`
chat on the PACKAGED host; the target is the kernel's own `create_agent` (`**meta`), so the
payload's `title` is visible on the record of the agent it makes.

- The routing form (`<routing>` with a label `<title>`, `<payload>` with its own `<title>`): the new
  agent holds the payload's title, and the label is nowhere on it.
- The inline form of the same call: `title` there is routing, lifted out and never sent, so the new
  agent has no title. (A verb that documents `title` is refused loud instead; the unit twin is
  `fantastic-ai-core` `tests.rs` `a_routing_name_in_the_payload_of_a_routed_call_is_delivered`.)

Run (when the owner runs e2e): `--only-free --id payload_routing_name`.
