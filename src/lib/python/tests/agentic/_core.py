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


from _engine import (
    MINTED_ID,
    HydrationError,
    check,
    dehydrate,
    denial_side,
    denied,
    reach,
    reboot,
)


def _built(h) -> bool:
    """Does this hydration have a tree to assert against at all?

    A config declaring `refused_at_write` has no tree to match, no edges to walk
    and no roster to close — PC-5 is its whole contract, and the other four are
    not failures, they are inapplicable."""
    if h.config.expect.hydration != "ok":
        return False
    if h.outcome() != "ok":
        raise HydrationError(
            f"{h.config.name}: declared hydration ok but got"
            f" {h.outcome()!r} — {h.outcome_error()!r}"
        )
    return True


# ─── PC-5 · the hydration verdict, on the loud channel ──────────


async def core_the_hydration_verdict_matches_the_declaration(h):
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


async def core_the_running_tree_matches_the_declared_shape(h):
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
    if not _built(h):
        return
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


async def core_every_declared_edge_is_reachable_and_every_refusal_names_its_side(h):
    """PC-2 is now a property of EXECUTING the spec, not a second walk.

    The config used to carry an `edges:` list that this re-walked. With the steps
    DSL the reach steps ARE the declared edges, so the executor asserts them in
    order — and it adds the one thing a bare verdict cannot carry: every denial
    must teach how to open it (`hint`). A refusal that cannot be acted on costs
    the same as no reply.

    What stays here is the GUARD: a spec that declares no reach at all has
    nothing to say about reachability, and that must be visible rather than
    passing silently.
    """
    if not _built(h):
        return
    reaches = [s for s in h.config.expect.steps if s.kind == "reach"]
    check(
        "PC-2",
        bool(reaches),
        f"{h.config.name}: declares no `reach:` step — PC-2 has nothing to walk",
    )


# ─── PC-3 · the roster is closed after boot ─────────────────────


async def core_the_roster_is_closed_after_boot(h):
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
    if not _built(h):
        return
    cfg = h.config
    sender = cfg.expect.closed_roster_sender or next(iter(cfg.roster_ids), None)
    if sender is None:
        return
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


async def core_a_reboot_rehydrates_the_identical_roster(h):
    """Persist, drop the kernel, bootstrap the same directory, run the production
    boot pass. `read_tree` hydrates the persisted children FIRST, so by the time
    a loader's `boot` runs every declared id already exists and the whole roster
    reports as `skipped` — that skip-by-id IS the idempotency mechanism, and it
    is why an id-less entry (which can never be skipped) duplicates on every
    single boot as a fresh `<bundle>_<hex6>`.

    The assertion is the roster itself, before and after. `Kernel.load` fires no
    boot, so the pass mirrors `longrun._boot_all_agents` — minus the root, whose
    boot would start the debounced flush loop."""
    if not _built(h):
        return
    if not h.config.expect.reboot:
        return

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
    # The reboot REPLACED the kernel; hand it back or every later
    # step would run against a tree that is no longer live.
    return h2


# ─── the CORE PHASE — PC-1..PC-5 on every config, by construction ─


# The order is deliberate. PC-4 REBOOTS, which replaces the kernel, so it runs
# LAST — the scenario's own steps must act on the hydration they were written
# against, not on a rebuilt one.
CORE_BEFORE = (
    core_the_hydration_verdict_matches_the_declaration,
    core_the_running_tree_matches_the_declared_shape,
    core_every_declared_edge_is_reachable_and_every_refusal_names_its_side,
    core_the_roster_is_closed_after_boot,
)
CORE_AFTER = (core_a_reboot_rehydrates_the_identical_roster,)


async def run_core(h, phases):
    """Run one group of core phases, threading the hydration through — a phase
    that rebuilds the kernel returns the new one."""
    for phase in phases:
        result = await phase(h)
        if result is not None:
            h = result
    return h
