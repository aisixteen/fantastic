"""`two_sided_gate` — an edge exists only if BOTH halves grant it.

The gate in `Agent.send` checks an agent-originated call TWICE: **egress**, the
SENDER's `can_call`, then **ingress**, the TARGET's `callable_by`
(`kernel/_agent.py:444-499`). Neither half is a permission on its own — they are
the two ends of one edge, and the edge is their intersection, verb by verb.

Why a manifest scenario, when `tests/kernel/test_call_security.py` already pins
the gate imperatively: a coding agent does not call `create_agent`, it AUTHORS a
roster, and the one-half mistake is invisible in that medium. `static_loader`'s
own fixture is the proof — its manifest grants `worker.can_call = {memory: [set,
read]}` and nothing else, and
`static_loader/tests/test_static_loader.py::test_worker_may_use_its_declared_edge_but_not_others`
is RED because the send dies at ingress. The field says reachable, the system
says otherwise, and nothing between the two says a word. So every claim here is
made by ATTEMPTING the call.

What this file asserts that the general runner (`test_manifest_contract.py`)
cannot:

- **the reconciler** — every declared `(sender, target, verb)` triple in the
  manifest, reconciled against what the caller actually gets. The runner walks
  the edges a config chose to list; this walks the grant maps themselves, so a
  verb somebody granted and never listed is still checked.
- **the ladder** — one pair, three rungs, opening one half at a time. A static
  edge list cannot express causality: only a mutation shows that each half is
  independently necessary and that the two together are sufficient.
- **the sweep** — the reachability matrix over the WHOLE roster, both
  directions. A list of edges can only confirm the edges you thought of; n²
  probes are what catch an edge nobody declared.
- **the repair path** — the missing half is supplied from ABOVE; a peer cannot
  reach through the parent it shares to write it.

Every callee is a `file_bridge` opened at its own io leg (`ingress_rule:
allow_all_ingress`), deliberately: a SEALED leg denies with the same
`reason: "unauthorized"` and no `side` (`file_bridge/tools.py:453-458`), which
would make an io seal indistinguishable from an egress refusal. The call gate is
the only gate left that can refuse, so `side` means exactly one thing here.
"""

from __future__ import annotations

import pytest

from _engine import check, dehydrate, denial_side, denied, reach, reboot

SCENARIO = "two_sided_gate"

# The payload for a verb under probe. `list` is read-only; `mkdir` is the WRITE
# probe — it is never inside a declared intersection in this roster, so it never
# runs, and "the directory is not there" is how a denial proves the handler was
# never reached.
_ARGS: dict[str, dict] = {"list": {"path": ""}, "mkdir": {"path": "probe_dir"}}


def _payload(verb: str, path: str | None = None) -> dict:
    args = dict(_ARGS.get(verb, {}))
    if path is not None and "path" in args:
        args["path"] = path
    return {"type": verb, **args}


def _specs(cfg) -> dict[str, dict]:
    """The manifest entries, by id. This is the DECLARATION — the thing the
    running system is reconciled against, never evidence about it."""
    return {
        spec["id"]: spec
        for ln in cfg.loaders
        for spec in ln.declared_agents
        if spec.get("id")
    }


def _verbs(spec: dict, field: str, other: str) -> set[str]:
    """The verbs `spec` declares for `other` on `can_call`/`callable_by`.

    A `"*"` value is refused rather than interpreted: a wildcard makes the
    per-verb intersection unobservable, and this scenario exists to observe it.
    Structural tokens (`$parent` …) are not this function's business — they carry
    no lateral reach and the sweep covers them."""
    grant = (spec.get(field) or {}).get(other)
    if grant is None:
        return set()
    if not isinstance(grant, list):
        pytest.fail(
            f"{SCENARIO}: {spec['id']}.{field}[{other}] is {grant!r} — this scenario"
            " declares explicit verb lists so the intersection is observable per verb"
        )
    return set(grant)


def _structural_pairs(children: dict[str, list[str]], universe: set[str]) -> set:
    """The edges the kernel stamps on every agent and nobody declares: `$self`,
    `$parent`, `$children` (`kernel/_authz.py:63-70`) — so each agent, plus each
    parent↔child pair, in both directions. Derived from the RUNNING tree, which
    PC-1 has already reconciled against the manifest."""
    pairs = {(a, a) for a in universe}
    for parent, kids in children.items():
        for kid in kids:
            if parent in universe and kid in universe:
                pairs |= {(parent, kid), (kid, parent)}
    return pairs


async def _adjacency(kernel, universe: list[str], verb: str) -> tuple[set, dict]:
    """Probe every ordered pair for one verb. Returns (reachable pairs, denial
    side per denied pair). Reachable means NOT denied: a pass returns the
    handler's own reply, which may be an error of its own (`file_bridge` has no
    `list` on a loader) — a different animal from a refusal."""
    reachable: set = set()
    sides: dict = {}
    for sender in universe:
        for target in universe:
            reply = await reach(kernel, sender, target, _payload(verb))
            if denied(reply):
                sides[(sender, target)] = denial_side(reply)
            else:
                reachable.add((sender, target))
    return reachable, sides


async def _open_half(kernel, agent_id: str, field: str, other: str, verbs: list[str]):
    """The operator's repair: MERGE one lateral grant into an existing map.

    Merged, not replaced, and that is not politeness — `update_agent` writes the
    field verbatim, so a bare `{other: verbs}` silently drops the structural
    baseline off the live record and severs the agent from its own parent. That
    defect is pinned below in
    `test_opening_one_lateral_half_does_not_silently_close_the_structural_ones`;
    here it would just be a confounder."""
    merged = dict((kernel.ctx.get(agent_id) or {}).get(field) or {})
    merged[other] = verbs
    return (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {"type": "update_agent", "id": agent_id, field: merged},
                }
            ]
        )
    )[0]["result"]


# ─── the reconciler: declaration → behaviour, verb by verb ──────


async def test_the_manifest_grants_reconcile_verb_by_verb_with_what_a_caller_gets(
    hydrate_scenario,
):
    """For every pair either end of the manifest mentions, the edge's verb set is
    the INTERSECTION of the two declared lists, and a verb outside it is refused
    by the half that omitted it.

    This is the reconciliation step the borrowed corpora do not have: openfga and
    a2a-tck both assert behaviour with a declarative file as INPUT, never that a
    declared shape equals a running one. Walking the grant maps rather than the
    config's `edges:` list is what makes it a reconciliation — a verb somebody
    granted and forgot to list as an expectation is still probed.

    The two halves are checked in order, so the side is not symmetric bookkeeping:
    a verb missing from the SENDER's list is refused at `egress` even when the
    callee would have accepted it (measured on `both_grant`, which accepts
    `mkdir` from a caller that was never granted it)."""
    h = await hydrate_scenario(SCENARIO)
    specs = _specs(h.config)
    probed = 0

    for sender, s_spec in specs.items():
        for target, t_spec in specs.items():
            if sender == target:
                continue
            egress = _verbs(s_spec, "can_call", target)
            ingress = _verbs(t_spec, "callable_by", sender)
            for verb in sorted(egress | ingress):
                probed += 1
                reply = await reach(h.kernel, sender, target, _payload(verb))
                both = verb in egress and verb in ingress
                # Which claim this triple pins: with one half of the pair entirely
                # undeclared it is the quadrant (TSG-1); with both halves naming
                # SOMETHING it is the verb intersection (TSG-3).
                check(
                    "TSG-3" if (egress and ingress) else "TSG-1",
                    denied(reply) != both,
                    f"{sender} -{verb}-> {target}: declared egress={sorted(egress)}"
                    f" ingress={sorted(ingress)} — expected"
                    f" {'reachable' if both else 'refused'}, got {reply!r}",
                )
                if both:
                    continue
                want = "egress" if verb not in egress else "ingress"
                check(
                    "TSG-2",
                    denial_side(reply) == want,
                    f"{sender} -{verb}-> {target}: the half that omitted {verb!r} is"
                    f" {want!r}, the denial named {denial_side(reply)!r}",
                )
    # A guard on the WALK, not a claim about the kernel: a manifest edited down to
    # nothing would otherwise reconcile perfectly against no probes at all.
    assert probed >= 6, f"{SCENARIO}: only {probed} declared triples to walk"


# ─── the ladder: each half is independently necessary ───────────


async def test_each_half_is_necessary_and_only_the_two_together_open_the_edge(
    hydrate_scenario,
):
    """One pair, opened one half at a time. `neither_grants` starts with neither.

    Rung 0 — refused, `side: "egress"`, and the callee's handler never ran: the
    `mkdir` it would have executed left no directory. Egress is decided FIRST
    (`kernel/_agent.py:453-472`), so at this rung the target is never consulted
    at all.

    Rung 1 — the operator grants the SENDER's half only. The same pair is still
    refused, and the denial has MOVED to `ingress`: the second half was refusing
    independently the whole time, invisible behind the first. This is the rung a
    declared edge list cannot express — the config can state a verdict, not a
    cause.

    Rung 2 — the operator grants the callee's half too, and only now does the
    edge exist. Necessary separately, sufficient together."""
    h = await hydrate_scenario(SCENARIO)
    k = h.kernel
    sender, target = "caller", "neither_grants"

    r0 = await reach(k, sender, target, _payload("list"))
    check("TSG-1", denied(r0), f"an undeclared edge was open: {r0!r}")
    check(
        "TSG-2",
        denial_side(r0) == "egress",
        f"with neither half declared the SENDER's half answers first: {r0!r}",
    )

    # The refusal is not a quiet no-op: the write it carried never happened.
    w = await reach(k, sender, target, _payload("mkdir", "never_created"))
    check("TSG-2", denied(w), f"a write on an undeclared edge went through: {w!r}")
    seen = (await k.send([{"target": target, "payload": _payload("list")}]))[0][
        "result"
    ]  # operator — ungated by origin
    names = {f["name"] for f in seen.get("files", [])}
    check(
        "TSG-2",
        "never_created" not in names,
        f"the callee's handler ran behind an egress denial: {sorted(names)}",
    )

    await _open_half(k, sender, "can_call", target, ["list"])
    r1 = await reach(k, sender, target, _payload("list"))
    check("TSG-1", denied(r1), f"the sender's half alone opened the edge: {r1!r}")
    check(
        "TSG-2",
        denial_side(r1) == "ingress",
        f"the second half was refusing all along, behind the first: {r1!r}",
    )

    await _open_half(k, target, "callable_by", sender, ["list"])
    r2 = await reach(k, sender, target, _payload("list"))
    check("TSG-1", not denied(r2), f"both halves granted and the edge is shut: {r2!r}")


# ─── the sweep: nothing else is reachable, either direction ─────


async def test_no_edge_exists_that_the_manifest_did_not_declare(hydrate_scenario):
    """The whole roster, every ordered pair, both verbs. The reachable set must be
    exactly the structural baseline plus the manifest's own grants — nothing
    rides along.

    A per-edge expectation can only confirm the edges its author thought of. This
    is the same argument as PC-1's reverse direction (a loader must own nothing
    the manifest did not name), moved from the tree to the call graph.

    Two properties fall out of the sweep that no single probe shows:

    - **co-residency grants nothing.** Six agents on one loader's roster, and not
      one sibling pair is reachable except where the manifest names both halves.
    - **authority is not transitive.** The root reaches the loader (its child) and
      stops; `(kernel_state, caller)` is refused, though `caller` is inside the
      subtree the root owns on disk. `$children` is direct children, a set
      membership, not a subtree.

    Sweeping a SECOND verb is what separates an edge from a permission: the two
    `list` edges vanish under `reflect`, which nothing grants. Discovery is gated
    like any other verb (`kernel/_agent.py:421-425`, nothing exempt), so two
    agents with a working edge still cannot see each other."""
    h = await hydrate_scenario(SCENARIO)
    d = await dehydrate(h.kernel)
    universe = [h.kernel.id, *h.config.loader_ids, *h.config.roster_ids]
    check(
        "TSG-4",
        set(universe) <= set(d["ids"]),
        f"the sweep universe is not running: {sorted(set(universe) - set(d['ids']))}",
    )
    structural = _structural_pairs(d["children"], set(universe))

    for verb in ("list", "reflect"):
        declared = {
            (e.sender, e.target)
            for e in h.config.expect.edges
            if e.allow and e.send.get("type") == verb
        }
        want = structural | declared
        got, sides = await _adjacency(h.kernel, universe, verb)
        check(
            "TSG-4",
            got == want,
            f"verb {verb!r}: undeclared edges {sorted(got - want)},"
            f" declared-and-missing {sorted(want - got)}",
        )
        check(
            "TSG-4",
            all(s in ("egress", "ingress") for s in sides.values()),
            f"verb {verb!r}: a refusal did not name a call-gate half —"
            f" {sorted({s for s in sides.values()})} (a sealed io leg denies with"
            " reason=unauthorized and NO side, which would be a different gate)",
        )


# ─── the repair path: the missing half comes from ABOVE ─────────


async def test_a_peer_cannot_supply_the_missing_half_and_an_operator_can(
    hydrate_scenario,
):
    """`caller` is refused by `only_caller_grants` at ingress, and the denial's
    hint tells it authority comes from ABOVE. It cannot act on that itself.

    Both routes are refused at EGRESS, which is the interesting part: `caller`
    holds `$parent: "*"` on the loader by baseline, and `update_agent` is
    PRIVILEGED — a wildcard never covers it (`kernel/_authz.py:73-77`), so "talk
    to your parent" never becomes "rewrite my sibling's grants through my
    parent". The root is not even a wildcard away: it is the caller's
    GRANDparent, and `$parent` does not reach it.

    Then the operator writes the same half, from outside the graph, and the edge
    opens — so the refusal was about WHO asked, not about the edit."""
    h = await hydrate_scenario(SCENARIO)
    k = h.kernel
    sender, target = "caller", "only_caller_grants"

    before = await reach(k, sender, target, _payload("list"))
    check("TSG-5", denied(before), f"the one-sided edge was open: {before!r}")
    check(
        "TSG-5",
        "ABOVE" in before.get("hint", ""),
        f"the denial must point at where authority comes from: {before!r}",
    )

    patch = {
        "type": "update_agent",
        "id": target,
        "callable_by": {sender: ["list"]},
    }
    for through in (h.config.loader_ids[0], k.id):
        r = await reach(k, sender, through, patch)
        check(
            "TSG-5", denied(r), f"{sender} rewrote a peer's grants via {through}: {r}"
        )
        check(
            "TSG-5",
            denial_side(r) == "egress",
            f"via {through}: a privileged verb is refused on the SENDER's half even"
            f" where a wildcard covers the ordinary ones — got {r!r}",
        )

    still = await reach(k, sender, target, _payload("list"))
    check("TSG-5", denied(still), f"a refused update_agent took effect: {still!r}")

    await _open_half(k, target, "callable_by", sender, ["list"])
    after = await reach(k, sender, target, _payload("list"))
    check("TSG-5", not denied(after), f"the operator's grant did not land: {after!r}")


# ─── the defect: a half-write eats the structural baseline ──────


@pytest.mark.xfail(
    strict=True,
    reason=(
        "DEFECT: update_agent writes can_call/callable_by verbatim, so opening one"
        " lateral half DELETES the structural baseline from the live record —"
        " {$self,$parent,$children} — and severs the agent from its own parent."
        " Agent.__init__ re-merges the baseline when the record is loaded, so the"
        " same record yields a DIFFERENT graph before and after a reboot."
    ),
)
async def test_opening_one_lateral_half_does_not_silently_close_the_structural_ones(
    hydrate_scenario,
):
    """Measured, and this is the whole failure:

        callable_by before: {'$self':'*', '$parent':'*', '$children':'*'}
        update_agent id=only_caller_grants callable_by={'caller':['list']}
        callable_by after:  {'caller': ['list']}
        gate_loader -reflect-> only_caller_grants
          → {'reason':'unauthorized', 'side':'ingress'}   # its own PARENT
        …after a reboot of the SAME record → the reflect goes through.

    The baseline is documented as stamped and MERGED on every agent, root,
    created and disk-loaded alike (`kernel/_authz.py:56-70`, CLAUDE.md "the
    structural baseline"). `Agent.__init__` honours that; `_verb_update_agent`
    (`kernel/_agent.py:1243-1251`) does not — it stores the patch as given. So an
    operator opening ONE lateral edge silently closes every structural one, and
    the agent it just wired can no longer be reached by the parent that would
    have to repair it. A reboot then quietly restores the grants, which is the
    tell: the live process and the persisted record disagree about the graph.

    This test asserts the CORRECT behaviour — the baseline survives a lateral
    write, and the live graph matches the rebooted one — so it goes green the day
    the merge moves into `update_agent` (or the baseline stops living in the
    record at all). `strict=True`: an unexpected pass is a failure, so nobody
    fixes this without retiring the marker."""
    h = await hydrate_scenario(SCENARIO)
    k = h.kernel
    loader, target = h.config.loader_ids[0], "only_caller_grants"

    before = await reach(k, loader, target, {"type": "reflect"})
    check(
        "TSG-6", not denied(before), f"the baseline parent edge is missing: {before!r}"
    )

    # The natural write — exactly what the README's grammar and every operator
    # example show, one lateral half, nothing else mentioned.
    (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "update_agent",
                        "id": target,
                        "callable_by": {"caller": ["list"]},
                    },
                }
            ]
        )
    )[0]["result"]

    live = await reach(k, loader, target, {"type": "reflect"})
    check(
        "TSG-6",
        not denied(live),
        f"opening a lateral half closed the structural one: {loader} -reflect->"
        f" {target} now {live!r}",
    )

    h2 = await reboot(h)
    rebooted = await reach(h2.kernel, loader, target, {"type": "reflect"})
    check(
        "TSG-6",
        denied(live) == denied(rebooted),
        f"the same record is a different graph across a reboot: live={live!r}"
        f" rebooted={rebooted!r}",
    )
