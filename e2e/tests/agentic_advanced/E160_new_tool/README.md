# `new_tool` (E160) — the model writes a new agent and uses it as a tool

Costs turns. Evidence for #727: tools are not a fixed set. The model is given a need
no agent meets — a verb that computes a SHA-256 hex digest — and a token minted at
test time, so the digest cannot be known without running code.

The fixture gives the AI `reflect` and `reload_bundle` on the root, a `create` line
on itself (every bundle: the folder bundle the model writes has no name yet, so the
line cannot name it), a file agent `bundles` serving `.fantastic/bundles/`, and the
human (`user`). No exec agent, no `*` on the root. The task names none of the route.

Scored (hard): a new agent of a py_agent folder bundle exists; the model called a verb
that bundle declares (its `bundle.yaml`), on it, and the reply carried the digest; the
model's turns carry the digest; the lines are exact (`grant_faults`: the AI's `can`
after the run is the one it started with, nothing in the new agent's `can` hands out
`*`, and the new agent is under the AI, so the AI reaches it by position). Reported:
`reload_bundle`, a docs reflect of the new agent (`tier`/`verb`/`part`) before its
first call (with `part`), turns.

The free twin `new_tool_twin` (E162) scripts the same sequence without a model and
runs this probe's `score` on it.

```sh
cd src/lib/python
BACKEND=… uv run python ../../../e2e/run.py --id new_tool
```
