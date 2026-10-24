# `call_gate` (E114) — the two-sided deny-all call gate, over the wire

Free: no model. `user_probe` agents (`send` dispatches AS the agent) under the
REST leg on the packaged host of `RUNTIME`, so each call under test has a real
agent sender. It walks the truth table (neither half → egress, caller's half →
ingress, both → passes, one-way), verb-level grants (`["reflect"]` vs `"*"`),
the `$self`/`$parent`/`$subtree` tokens relative to the holder, the REST leg's
own egress, no self-raise (`update_agent` on itself refused, record unchanged),
the grant-shape refusal at the write (`invalid_meta`), and `delete_agent`
pruning the deleted subtree's ids from other agents' grants so a re-created id
inherits no stale edge. The in-process contract (`contract/agentic/specs/two_sided_gate.yaml`)
covers the static truth table; this adds the wire, the tokens and pruning.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id call_gate          # all three
```
