# `call_gate` (E114) — the call gate on `can` lines, over the wire

Free: no model. `user_probe` agents (`send` dispatches AS the agent) under the
REST leg on the packaged rust host, so each call under test has a real agent
caller. A call passes on the caller's own `can` plus every ancestor's cap; the
target keeps no list and can neither widen nor block it. It walks: one line on
the caller (none → the unknown answer, byte-identical to a missing id; a line →
passes, the target's record unchanged; one-way), the levels (`call: [reflect]`,
`call: [*]`, `update` includes `call`), position (own tree, no line), `$parent`
(the parent needs a written line), siblings, a line reaching the target's tree, the writer subset (`rest` hands out only what it holds),
the cap (an ancestor that does not hold the target cuts the call), the REST leg
as an agent (no covering line → unknown, no `create` line on the root → unknown),
never upward (no write of one's own `can` nor of the parent's record), the
line shape at the write (`invalid_meta`; a missing id written by an agent →
unknown), and ids reusable (a line survives its target's delete, a new line on
an absent id is refused, a re-created id is reached with no write, a delete
takes the subtree). The kernel's `authz` unit tests cover the solver's table;
this adds the wire.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id call_gate:rust
```
