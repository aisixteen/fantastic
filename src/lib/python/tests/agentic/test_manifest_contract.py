"""The GENERAL runner — the five preconfig invariants, for every config.

One question, asked of every `configs/*.yaml`: **does the running kernel match
the shape the file declared?** Nothing here knows what any individual scenario is
about. Scenario-specific claims (cancel semantics, the error taxonomy, sealed
legs) belong in that scenario's own validator; this file would go stale the
moment it started asserting them.

The five, in the order a hydration produces them:

- **PC-5** the hydration VERDICT — did the declared roster become a record at
  all, and on which channel did it refuse? Asserted for every config, including
  the healthy ones, because "was not refused" is half the invariant.
- **PC-1** shape — every declared agent exists where it was declared, and the
  declaring parent owns NOTHING ELSE. Both directions, or a stray agent rides
  along unnoticed.
- **PC-2** reachability — every declared edge is usable and every refused one
  names the half that refused. Grant maps are never READ: a field check stopped
  tracking the property the moment a second gate existed.
- **PC-3** closure — after boot the roster is fixed. An agent cannot create
  another, and cannot raise its own authority to get there.
- **PC-4** reboot — the same tree comes back, once, from disk.

Every assertion is tagged with the requirement it pins (`check`), so a run
reports a coverage matrix rather than a pass count.
"""

from __future__ import annotations

import pytest

from _engine import (
    MINTED_ID,
    check,
    dehydrate,
    denial_side,
    denied,
    reach,
    reboot,
)


def _require_built(h) -> None:
    """Skip a test whose invariant only means something on a kernel that was
    actually built. A config declaring `refused_at_write` has no tree to match,
    no edges to walk and no roster to close — PC-5 is its whole contract."""
    if h.config.expect.hydration != "ok":
        pytest.skip(
            f"{h.config.name}: declares hydration={h.config.expect.hydration!r}"
            " — only PC-5 applies"
        )
    if h.outcome() != "ok":
        pytest.fail(f"{h.config.name}: hydration failed — {h.outcome_error()!r}")


# ─── PC-5 · the hydration verdict, on the loud channel ──────────


async def test_the_hydration_verdict_matches_the_declaration(hydrated_config):
    """A bad manifest is refused at WRITE time — but only the kind that cannot be
    READ. Measured: `manifest: nope.yaml` is refused by `validate_meta` and never
    becomes a record; a manifest that parses and then names a bad ENTRY produces
    a clean-looking create reply, a silently PARTIAL roster, and no complaint
    anywhere, because `_verb_create_agent` boots the child, DISCARDS the reply
    and swallows the exception (`kernel/_agent.py:1231-1236`) while
    `static_loader._boot` never raises — it RETURNS `{"error": …, booted: False}`.

    So the verdict has two channels and the config names which one it expects:
    `refused_at_write` (the create reply) or `boot_error` (the explicit re-boot).
    Asserting only the happy channel would greenlight a half-built kernel, which
    is the exact failure this suite exists to catch."""
    h = hydrated_config
    want = h.config.expect.hydration
    got = h.outcome()
    check(
        "PC-5",
        got == want,
        f"{h.config.name}: declared {want!r}, got {got!r}"
        f" (creates={h.creates!r} boots={h.boots!r})",
    )
    if want == "ok":
        for lid in h.config.loader_ids:
            check("PC-5", h.creates[lid].get("id") == lid, f"{lid}: {h.creates[lid]!r}")
            check(
                "PC-5",
                h.loader_boot(lid).get("booted") is True,
                f"{lid}: an explicit boot is the ONLY loud channel"
                f" — {h.loader_boot(lid)!r}",
            )
        return

    err = h.outcome_error()
    check(
        "PC-5",
        h.config.expect.error_contains in err,
        f"refusal must name why: {h.config.expect.error_contains!r} not in {err!r}",
    )
    if want == "refused_at_write":
        for lid in h.config.loader_ids:
            rec = h.creates[lid]
            # The write-time refusal shape: no id, `created: False`, and the
            # agent never enters the roster at all.
            check("PC-5", rec.get("reason") == "invalid_meta", f"{lid}: {rec!r}")
            check(
                "PC-5",
                "id" not in rec,
                f"{lid}: refused create must mint no id: {rec!r}",
            )
            check(
                "PC-5",
                lid not in h.agents,
                f"{lid} was refused yet exists in the roster",
            )
    else:  # boot_error — created, reported as a normal success, silently inert
        broken = [
            lid
            for lid in h.config.loader_ids
            if h.loader_boot(lid).get("booted") is not True
        ]
        check("PC-5", bool(broken), "declared boot_error but every loader booted green")
        for lid in broken:
            check(
                "PC-5",
                h.creates[lid].get("id") == lid,
                f"{lid}: a boot failure is SWALLOWED at create — the create reply"
                f" is expected to look clean, got {h.creates[lid]!r}",
            )


# ─── PC-1 · declared shape == running tree, both directions ─────


async def test_the_running_tree_matches_the_declared_shape(hydrated_config):
    """Read back through ONE root reflect, not through private attributes — the
    suite reconciles against what the system SAYS about itself.

    Both directions matter. Forward catches an entry that never got created
    (`_boot` returns on the FIRST bad spec, so everything after it silently does
    not exist). Reverse catches an extra child — including the one the manifest
    cannot see: an id already taken elsewhere is `skipped` by id with no report,
    so a declared `handler_module` can quietly never take effect.

    Parentage is asserted, not assumed: every manifest entry is created as a
    child of the LOADER (`static_loader/tools.py:106`), and a `parent:` key in a
    spec crashes `Agent.create` inside the swallowed boot."""
    h = hydrated_config
    _require_built(h)
    d = await dehydrate(h.kernel)
    agents, children = d["agents"], d["children"]

    for ln in h.config.loaders:
        check(
            "PC-1",
            ln.id in agents,
            f"loader {ln.id} is not in the running tree: {d['ids']}",
        )
        if not ln.declares_roster:
            continue
        for spec in ln.declared_agents:
            aid = spec.get("id")
            check(
                "PC-1",
                bool(aid),
                f"{ln.id}: an id-less entry duplicates on EVERY boot: {spec!r}",
            )
            node = agents.get(aid)
            check(
                "PC-1", node is not None, f"{aid} declared by {ln.id} but not running"
            )
            check(
                "PC-1",
                node["handler_module"] == spec["handler_module"],
                f"{aid}: declared {spec['handler_module']!r}, running"
                f" {node['handler_module']!r}",
            )
            check(
                "PC-1",
                node["parent_id"] == ln.id,
                f"{aid}: a manifest entry is a child of its LOADER — parent is"
                f" {node['parent_id']!r}, expected {ln.id!r}",
            )
        # reverse: the loader owns the declared roster and nothing besides.
        check(
            "PC-1",
            set(children.get(ln.id, [])) == h.config.expected_children(ln.id),
            f"{ln.id} owns {sorted(children.get(ln.id, []))},"
            f" declared {sorted(h.config.expected_children(ln.id))}",
        )


# ─── PC-2 · reachability, never a config field ──────────────────


async def test_every_declared_edge_is_reachable_and_every_refusal_names_its_side(
    hydrated_config,
):
    """The config declares edges as PROBES, not as grant maps, and this walks
    them with `reach()` — an agent-origin send.

    Reading `can_call` would prove nothing. An agent whose `can_call` names a
    target is still refused when the target's `callable_by` does not name it
    back; measured on `static_loader`'s own fixture, where the manifest granted
    `worker.can_call = {memory: [set, read]}` and the send came back
    `{"side": "ingress", "reason": "unauthorized"}` — the field was set and the
    operation was unreachable. `side` is the evidence the gate is two-sided at
    all, so a denial that declares one must match it.

    An ALLOW is asserted as a NEGATIVE (`not denied`): a pass returns the
    handler's own reply, which may itself be an error — a sealed leg, a missing
    store — and that is a different animal from a refusal."""
    h = hydrated_config
    _require_built(h)
    edges = h.config.expect.edges
    if not edges:
        pytest.skip(f"{h.config.name}: declares no edges — PC-2 has nothing to walk")
    for edge in edges:
        reply = await reach(h.kernel, edge.sender, edge.target, edge.send)
        if edge.allow:
            check("PC-2", not denied(reply), f"{edge}: declared usable, got {reply!r}")
            continue
        check("PC-2", denied(reply), f"{edge}: declared refused, got {reply!r}")
        check(
            "PC-2",
            bool(reply.get("hint")),
            f"{edge}: a denial must teach how to open it",
        )
        if edge.side:
            check(
                "PC-2",
                denial_side(reply) == edge.side,
                f"{edge}: declared side {edge.side!r}, got {denial_side(reply)!r} — the"
                " half that refused is the whole claim about two-sidedness",
            )


# ─── PC-3 · the roster is closed after boot ─────────────────────


async def test_the_roster_is_closed_after_boot(hydrated_config):
    """The loader hydrates as OPERATOR at boot; that is not standing power. After
    boot every roster agent is deny-all like anything else, so the set of agents
    is FIXED by the manifest — closed by the transport's default, not by a mode
    flag.

    Two probes, because closure has two escape routes. Directly: `create_agent`
    on the root is refused at egress — PRIVILEGED verbs lose wildcard cover
    (`kernel/_authz.py:73-77`), so a baseline `$parent: "*"` never becomes graph
    mutation. Indirectly: an agent editing its OWN `can_call` to grant itself the
    reach — refused with `side: "self-grant"`, authority descends only.

    The refusal is then re-probed for EFFECT, not inspected as a field: after the
    attempted self-grant a previously-denied edge must still deny."""
    h = hydrated_config
    _require_built(h)
    cfg = h.config
    sender = cfg.expect.closed_roster_sender or next(iter(cfg.roster_ids), None)
    if sender is None:
        pytest.skip(f"{cfg.name}: declares no roster agent to probe closure from")
    check(
        "PC-3", sender in h.agents, f"closure probe {sender!r} is not a running agent"
    )

    before = h.ids
    r = await reach(
        h.kernel,
        sender,
        h.kernel.id,
        {
            "type": "create_agent",
            "parent_id": h.kernel.id,
            "handler_module": "file_bridge.tools",
            "id": "smuggled",
        },
    )
    check("PC-3", denied(r), f"{sender} could create_agent on the root: {r!r}")
    check("PC-3", denial_side(r) == "egress", f"expected an egress refusal, got {r!r}")
    check(
        "PC-3",
        h.ids == before,
        f"a refused create changed the roster: {before} → {h.ids}",
    )

    s = await reach(
        h.kernel,
        sender,
        sender,
        {
            "type": "update_agent",
            "id": sender,
            "can_call": {h.kernel.id: ["create_agent"]},
        },
    )
    check("PC-3", denied(s), f"{sender} raised its own authority: {s!r}")
    check(
        "PC-3",
        denial_side(s) == "self-grant",
        f"expected a self-grant refusal, got {s!r}",
    )

    # The refusal must have had no effect — asserted by reachability, not by
    # re-reading the field it declined to write.
    r2 = await reach(
        h.kernel,
        sender,
        h.kernel.id,
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "id": "smuggled",
        },
    )
    check("PC-3", denied(r2), f"the self-grant took effect after all: {r2!r}")


# ─── PC-4 · a reboot rehydrates the same tree, once ─────────────


async def test_a_reboot_rehydrates_the_identical_roster(hydrated_config):
    """Persist, drop the kernel, bootstrap the same directory, run the production
    boot pass. `read_tree` hydrates the persisted children FIRST, so by the time
    a loader's `boot` runs every declared id already exists and the whole roster
    reports as `skipped` — that skip-by-id IS the idempotency mechanism, and it
    is why an id-less entry (which can never be skipped) duplicates on every
    single boot as a fresh `<bundle>_<hex6>`.

    The assertion is the roster itself, before and after. `Kernel.load` fires no
    boot, so the pass mirrors `longrun._boot_all_agents` — minus the root, whose
    boot would start the debounced flush loop."""
    h = hydrated_config
    _require_built(h)
    if not h.config.expect.reboot:
        pytest.skip(f"{h.config.name}: declares reboot: false")

    before = h.ids
    minted_before = [i for i in before if MINTED_ID.match(i)]
    h2 = await reboot(h)
    after = h2.ids

    check(
        "PC-4", after == before, f"roster changed across a reboot: {before} → {after}"
    )
    minted_after = [i for i in after if MINTED_ID.match(i)]
    check(
        "PC-4",
        minted_after == minted_before,
        f"auto-minted ids appeared across the reboot ({minted_before} → {minted_after})"
        " — that is what an id-less manifest entry looks like",
    )
    for ln in h2.config.loaders:
        boot = h2.loader_boot(ln.id)
        check("PC-4", boot.get("booted") is True, f"{ln.id} did not re-boot: {boot!r}")
        check(
            "PC-4",
            boot.get("created") == [],
            f"{ln.id} CREATED agents on a reboot instead of skipping them: {boot!r}",
        )
        if ln.declares_roster:
            declared = {a["id"] for a in ln.declared_agents if a.get("id")}
            check(
                "PC-4",
                set(boot.get("skipped") or []) == declared,
                f"{ln.id}: skipped {sorted(boot.get('skipped') or [])},"
                f" declared {sorted(declared)}",
            )
