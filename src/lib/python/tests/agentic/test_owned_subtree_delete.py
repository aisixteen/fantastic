"""`owned_subtree_delete` — a delete takes the whole owned subtree and nothing else.

Two requirements, one shape. **C18**: deleting a loader removes every agent that
loader owns — record, sidecar and wiring — and leaves its neighbour untouched.
**C17**: a refused mutation mutates NOTHING, and the reply names what refused.

Ownership is the LOADER boundary, and that is a property of the manifest format
rather than a choice of layout: every entry is created as a child of its loader
(`static_loader/tools.py:106`), and a `parent:` key crashes `Agent.create`
inside `create_agent`'s swallowed boot. "The owned subtree" is therefore exactly
"one loader's roster", which is why the config declares TWO loaders — a
neighbour that is a sibling BRANCH would prove nothing about a manifest-declared
system, because a manifest cannot declare one.

The general runner already pins this config's shape, edges, closure and reboot
(PC-1…PC-5). Nothing here repeats that: every test starts from a hydration that
was already vetted (`hydrate_scenario` raises unless the declared outcome held)
and asserts only what a DELETE does to it.

Named for the invariant rather than for the kernel function it exercises, and
that is load-bearing: `tests/kernel/test_cascade_delete.py` already owns that
basename, and with no `__init__.py` anywhere under `tests/` two modules sharing
one basename abort the whole repo-wide collection with an import-file mismatch.
Every basename in this suite has to be unique repo-wide.

Two measured corrections to the canonical wording are baked in:

- *"a missing target is a no-op, a wrong state is an error"* is half true here.
  A missing TASK is an honest no-op (`{"cancelled": false, "killed": []}`); a
  missing AGENT is an ERROR (`{"error": "no agent 'ghost'", "deleted": false}`).
  Pinning the checkpoint-conformance pairing would pin a behaviour this kernel
  does not have.
- the in-RAM cascade does not touch disk AT ALL. Removing the directory is a
  SECOND mechanism, driven by the `removed` state event through the loader
  (`_FlushLoop._on_state`, `kernel_state/tools.py:447`) — a debounced background
  loop this harness deliberately does not run (`_engine`'s module docstring).
  The disk test drives the loader's own `forget_record` and asserts the two
  halves separately rather than pretending one call does both.
"""

from __future__ import annotations

from _engine import check, dehydrate, denial_side, denied, reach
from _testkit import persist

# The two rosters, by their loader. Read from the hydration's own config rather
# than restated here, so renaming an entry in the yaml can never leave a stale
# expectation passing in this file.
DOOMED, NEIGHBOUR = "l_a", "l_b"


def _roster(h, loader_id: str) -> list[str]:
    """The ids that loader owns, per the config, plus the loader itself — the
    exact set a cascade on it must consume."""
    return sorted(h.config.expected_children(loader_id) | {loader_id})


async def _answers(kernel, agent_id: str) -> bool:
    """Does this id name a live agent that answers?

    An OPERATOR send, deliberately: the question is EXISTENCE, not authorization.
    An agent-origin probe would conflate the two — target resolution runs BEFORE
    the gate (`kernel/_agent.py:415-421`), so a live-but-unreachable agent and a
    deleted one are told apart only by an ungated send."""
    reply = (await kernel.send([{"target": agent_id, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    return isinstance(reply, dict) and reply.get("id") == agent_id


async def _unknown(kernel, agent_id: str) -> bool:
    """The reply for an id that names no agent — the positive evidence of
    removal, as opposed to `not _answers()` which a broken handler also gives."""
    reply = (await kernel.send([{"target": agent_id, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    return reply.get("error") == f"no agent {agent_id!r}"


# ─── C18 · the cascade consumes exactly one loader's roster ─────


async def test_deleting_a_loader_takes_every_agent_it_owns_with_it(hydrate_scenario):
    """One `delete_agent` on the loader, and the entire declared roster is gone —
    not detached, not orphaned, GONE: every member answers `no agent`.

    Asserted per declared id, not as a set difference, because the cascade is
    recursive (`_cascade_delete`, `kernel/_agent.py:1303`) and a partial cascade
    — one child surviving its parent — is the failure this is looking for. A set
    comparison reports it as "the roster changed"; naming the id reports WHICH
    member outlived its owner."""
    h = await hydrate_scenario("owned_subtree_delete")
    k = h.kernel
    doomed = _roster(h, DOOMED)
    for aid in doomed:
        check("C18", await _answers(k, aid), f"{aid} was not running before the delete")

    reply = (
        await k.send(
            [{"target": k.id, "payload": {"type": "delete_agent", "id": DOOMED}}]
        )
    )[0]["result"]
    check("C18", reply == {"deleted": True, "id": DOOMED}, f"delete said {reply!r}")

    for aid in doomed:
        check(
            "C18",
            await _unknown(k, aid),
            f"{aid} outlived the cascade on {DOOMED} — a partial cascade leaves an"
            " agent with no owner, still dispatchable",
        )
    d = await dehydrate(k)
    check(
        "C18",
        not (set(doomed) & set(d["ids"])),
        f"the tree still reports {sorted(set(doomed) & set(d['ids']))}",
    )


async def test_the_neighbour_roster_keeps_its_agents_and_its_wiring(hydrate_scenario):
    """Sparing the neighbour is not "its records are still listed" — it is that
    the neighbour still WORKS. So the probe is its declared edge, walked with
    `reach()` after the cascade: `b_head → b_leaf` is a two-sided grant, and a
    delete that clobbered either half of it would leave both agents listed and
    the edge dead.

    The store is checked the same way and for the same reason: it is a root
    child, so a cascade that walked upward instead of downward would take the
    persistence provider with it, and the root reflect would go back to
    `provider: null` while every record still looked fine."""
    h = await hydrate_scenario("owned_subtree_delete")
    k = h.kernel
    before = await reach(k, "b_head", "b_leaf", {"type": "list", "path": ""})
    check("C18", not denied(before), f"the neighbour edge was dead already: {before!r}")

    (
        await k.send(
            [{"target": k.id, "payload": {"type": "delete_agent", "id": DOOMED}}]
        )
    )[0]["result"]

    for aid in _roster(h, NEIGHBOUR):
        check("C18", await _answers(k, aid), f"{aid} died with the {DOOMED} cascade")
    after = await reach(k, "b_head", "b_leaf", {"type": "list", "path": ""})
    check(
        "C18",
        not denied(after),
        f"the neighbour's declared edge broke when {DOOMED} was deleted: {after!r}",
    )
    d = await dehydrate(k)
    check("C18", await _answers(k, "store"), "the root-child store died with a subtree")
    check(
        "C18",
        d["persistence_provider"] == "store",
        f"persistence lost its provider: {d['persistence_provider']!r}",
    )


async def test_the_cascade_takes_a_members_sidecar_off_disk_with_its_record(
    hydrate_scenario,
):
    """The disk half, and it is TWO mechanisms, asserted as two.

    `_cascade_delete` removes an agent from process state and publishes a
    `removed` event; the directory goes when the loader acts on that event
    (`_FlushLoop._on_state`, `kernel_state/tools.py:447`, a 0.15s debounced loop
    this harness does not run). Measured here: after the cascade every
    `agents/l_a/**` file is still on disk. Asserting a one-call disk removal
    would pin a mechanism the kernel does not have.

    What the loader's own `forget_record` then proves is the shape of ownership:
    ONE rmtree of ONE directory is the whole owned subtree, sidecars included,
    because the layout is POSITIONAL —
    `agents/<loader>/agents/<member>/state.yaml` — not a naming convention. The
    neighbour's files cannot be inside it, so they cannot be caught by it."""
    h = await hydrate_scenario("owned_subtree_delete")
    k = h.kernel
    wrote = await reach(k, "a_head", "a_notes", {"type": "set", "key": "k", "value": 1})
    check("C18", wrote.get("set") is True, f"the sidecar was never written: {wrote!r}")
    persist(k)

    before = set((await dehydrate(k, disk=True))["disk_files"])
    sidecar = f"agents/{DOOMED}/agents/a_notes/state.yaml"
    check("C18", sidecar in before, f"no sidecar inside the doomed subtree: {before}")
    neighbour_files = {f for f in before if f.startswith(f"agents/{NEIGHBOUR}/")}
    check("C18", bool(neighbour_files), f"the neighbour persisted nothing: {before}")

    (
        await k.send(
            [{"target": k.id, "payload": {"type": "delete_agent", "id": DOOMED}}]
        )
    )[0]["result"]
    mid = set((await dehydrate(k, disk=True))["disk_files"])
    check(
        "C18",
        sidecar in mid,
        "the in-RAM cascade wrote to disk — removal is the loader's job, driven"
        f" by the `removed` event, not by `_cascade_delete`: {sorted(mid)}",
    )

    forget = (
        await k.send(
            [{"target": k.id, "payload": {"type": "forget_record", "id": DOOMED}}]
        )
    )[0]["result"]
    check("C18", forget == {"ok": True}, f"forget_record said {forget!r}")
    after = set((await dehydrate(k, disk=True))["disk_files"])
    left = {f for f in after if f.startswith(f"agents/{DOOMED}/")}
    check("C18", not left, f"the deleted subtree left files behind: {sorted(left)}")
    check(
        "C18",
        neighbour_files <= after,
        f"the neighbour lost {sorted(neighbour_files - after)} to a delete of"
        f" {DOOMED} — the owned dirs are not disjoint after all",
    )


# ─── C17 · a refused mutation mutates nothing ───────────────────


async def test_a_delete_lock_anywhere_in_the_subtree_refuses_the_whole_cascade(
    hydrate_scenario,
):
    """`b_vault` carries `delete_lock: true` and it is the deepest thing in
    `l_b`, so the refusal has to be found by a walk (`_find_locked_descendant`)
    before a single record is touched. The reply names BOTH ends — the id that
    was asked for and the id that refused — which is what "says which half
    refused" means for a cascade.

    "Mutates nothing" is asserted by reachability, never by a field: every member
    still answers, and the neighbour's declared edge still carries traffic. A
    cascade that ran halfway and then hit the lock would leave the records the
    walk had already passed, with the reply still reading `locked: true`."""
    h = await hydrate_scenario("owned_subtree_delete")
    k = h.kernel
    roster = _roster(h, NEIGHBOUR)

    reply = (
        await k.send(
            [{"target": k.id, "payload": {"type": "delete_agent", "id": NEIGHBOUR}}]
        )
    )[0]["result"]
    check("C17", reply.get("locked") is True, f"the lock let it through: {reply!r}")
    check("C17", reply.get("deleted") is not True, f"refused AND deleted: {reply!r}")
    check("C17", reply.get("blocked_by") == "b_vault", f"blocked_by: {reply!r}")
    check("C17", reply.get("id") == NEIGHBOUR, f"the reply lost the request: {reply!r}")
    check(
        "C17",
        "b_vault" in reply.get("error", "") and NEIGHBOUR in reply.get("error", ""),
        f"the error names one end but not both: {reply.get('error')!r}",
    )

    for aid in roster:
        check("C17", await _answers(k, aid), f"{aid} was consumed by a REFUSED delete")
    edge = await reach(k, "b_head", "b_leaf", {"type": "list", "path": ""})
    check("C17", not denied(edge), f"a refused delete broke the wiring: {edge!r}")

    # The lock refuses at its own id too — it is not merely an obstacle on the
    # way down to something else.
    direct = (
        await k.send(
            [{"target": k.id, "payload": {"type": "delete_agent", "id": "b_vault"}}]
        )
    )[0]["result"]
    check("C17", direct.get("locked") is True, f"the lock's own id: {direct!r}")
    check("C17", direct.get("blocked_by") == "b_vault", f"blocked_by: {direct!r}")
    check("C17", await _answers(k, "b_vault"), "the locked agent deleted itself")


async def test_clearing_the_lock_the_refusal_named_lets_the_same_cascade_through(
    hydrate_scenario,
):
    """The refusal ends `…clear it via update_agent before deleting`. Following
    that sentence literally must produce a working delete, or the reply is
    teaching a repair that does not repair.

    This is also the second half of "mutates nothing": the SAME cascade, replayed
    against the state the refusal left behind, consumes the full roster. If the
    blocked attempt had detached anything, the replay would come back
    `deleted: true` with a member still answering."""
    h = await hydrate_scenario("owned_subtree_delete")
    k = h.kernel
    roster = _roster(h, NEIGHBOUR)

    blocked = (
        await k.send(
            [{"target": k.id, "payload": {"type": "delete_agent", "id": NEIGHBOUR}}]
        )
    )[0]["result"]
    check("C17", blocked.get("locked") is True, f"nothing to clear: {blocked!r}")

    cleared = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "update_agent",
                        "id": "b_vault",
                        "delete_lock": False,
                    },
                }
            ]
        )
    )[0]["result"]
    check(
        "C17", cleared.get("updated") is True, f"the named repair failed: {cleared!r}"
    )

    reply = (
        await k.send(
            [{"target": k.id, "payload": {"type": "delete_agent", "id": NEIGHBOUR}}]
        )
    )[0]["result"]
    check(
        "C17",
        reply == {"deleted": True, "id": NEIGHBOUR},
        f"the refusal was not only about the lock: {reply!r}",
    )
    for aid in roster:
        check("C18", await _unknown(k, aid), f"{aid} outlived the unblocked cascade")


async def test_a_missing_task_is_a_no_op_while_a_missing_agent_is_an_error(
    hydrate_scenario,
):
    """The canonical pairing — "missing target is a silent no-op, wrong state is
    an error" — does NOT hold here, and this pins what does.

    Measured: `cancel` on a task id that was never minted returns
    `{"cancelled": false, "killed": []}` with no `error` key — an honest no-op,
    the kernel refusing to pretend it killed something. `delete_agent` on an id
    that names no agent returns `{"error": "no agent 'ghost'", "deleted": false}`
    — an ERROR, plus the live id index. Same for `update_agent`
    (`{"updated": false}`).

    That difference is not an inconsistency: a task id is a claim about work that
    may legitimately have finished a microsecond ago, while an agent id is a
    claim about the roster, which the caller can be told is wrong. The roster is
    re-read afterwards because "an error" and "an error plus a mutation" are the
    two things a failing delete could be."""
    h = await hydrate_scenario("owned_subtree_delete")
    k = h.kernel
    before = h.ids

    no_op = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {"type": "cancel", "task_id": "nobody:deadbeef"},
                }
            ]
        )
    )[0]["result"]
    check("C17", no_op == {"cancelled": False, "killed": []}, f"cancel: {no_op!r}")
    check("C17", "error" not in no_op, f"a missing task is not an error: {no_op!r}")

    gone = (
        await k.send(
            [{"target": k.id, "payload": {"type": "delete_agent", "id": "ghost"}}]
        )
    )[0]["result"]
    check("C17", gone.get("deleted") is False, f"delete of a missing id: {gone!r}")
    check("C17", gone.get("error") == "no agent 'ghost'", f"{gone!r}")

    patched = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {"type": "update_agent", "id": "ghost", "mode": "data"},
                }
            ]
        )
    )[0]["result"]
    check(
        "C17",
        patched.get("reason") == "unknown_agent",
        f"update of a missing id: {patched!r}",
    )
    check("C17", patched.get("error") == "no agent 'ghost'", f"{patched!r}")

    check("C17", h.ids == before, f"a refused mutation changed the roster: {h.ids}")


async def test_a_roster_agent_cannot_delete_its_sibling_through_its_own_loader(
    hydrate_scenario,
):
    """The gate half of "says which half refused", on the verb this scenario is
    about — the runner's PC-3 probes `create_agent` on the root, which is a
    different reach.

    `a_head`'s baseline grant is `{$self:*, $parent:*, $children:*}`, so its
    loader IS covered by a wildcard for ordinary verbs. `delete_agent` is
    PRIVILEGED and loses that cover unless the other end is the holder itself
    (`_verb_allowed`, `kernel/_authz.py`), so "talk to your parent" never becomes
    "have your parent delete your sibling". Measured: `a_head may not call
    'delete_agent' on l_a`, `side: egress` — the send never left the sender, so
    the loader was never even asked.

    Effect is then re-probed rather than the roster re-read: the sibling still
    answers on its DECLARED edge."""
    h = await hydrate_scenario("owned_subtree_delete")
    k = h.kernel

    through_parent = await reach(
        k, "a_head", DOOMED, {"type": "delete_agent", "id": "a_leaf"}
    )
    check("C17", denied(through_parent), f"a sibling was deletable: {through_parent!r}")
    check(
        "C17",
        denial_side(through_parent) == "egress",
        f"a privileged verb must die at the SENDER: {through_parent!r}",
    )
    check("C17", bool(through_parent.get("hint")), "a denial must teach the model")

    through_root = await reach(
        k, "a_head", k.id, {"type": "delete_agent", "id": "a_leaf"}
    )
    check("C17", denied(through_root), f"the root took the order: {through_root!r}")
    check(
        "C17",
        denial_side(through_root) == "egress",
        f"expected an egress refusal, got {through_root!r}",
    )

    survived = await reach(k, "a_head", "a_leaf", {"type": "list", "path": ""})
    check("C17", not denied(survived), f"the refusal had an effect: {survived!r}")
    check("C17", await _answers(k, "a_leaf"), "a refused delete removed the sibling")
