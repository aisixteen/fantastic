# `new_tool` (E160) — the model writes a new agent and uses it as a tool

Costs turns. Evidence for #727: tools are not a fixed set. The model is given a need
no agent meets — a verb that computes a SHA-256 hex digest — and a token minted at
test time, so the digest cannot be known without running code.

The fixture gives the AI the kernel verbs that make an agent (`reflect`, `docs`,
`reload_bundle`, `create_agent`, `update_agent` on the root), a file agent `bundles`
serving `.fantastic/bundles/`, and the human (`user`). No exec agent, no `$subtree`,
no `*` on the root. The task names none of the route.

Scored (hard): a new agent of a py_agent folder bundle exists; the model called a verb
that bundle declares, on it, and the reply carried the digest; the model's turns carry
the digest; the grants are exact (`grant_faults`: both halves of the gate name the
verb, nothing on the new agent grants `*`, nothing the run wrote into the AI's
`can_call` grants `*`). Reported: `reload_bundle`, `docs` of the new agent before its
first call (with `part`), turns.

The free twin `new_tool_twin` (E162) scripts the same sequence without a model and
runs this probe's `score` on it.

```sh
cd src/lib/python
BACKEND=… uv run python ../../../e2e/run.py --id new_tool
```
