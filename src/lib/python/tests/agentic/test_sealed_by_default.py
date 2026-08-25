"""`sealed_by_default` — a freshly declared io leg denies; opening it is an act.

Pins **C23**: an undeclared capability ERRORS. Not a silent no-op, not a partial
success, not a RAM fallback — the caller is told, and nothing moved.

The config is built so every refusal is attributable. It declares NO grants at
all: every roster agent is a child of the loader, so the structural baseline
(`$parent`/`$children`) already opens the CALL gate on each loader→leg edge. Five
`file_bridge` legs then differ ONLY in posture — three of them (`sealed_fs`,
`ro`, `rw`) serve the SAME `data/` directory — so a denial can be attributed to
one declared fact and to nothing else, and the open sibling is a witness that no
byte moved behind the refusal.

`vault` (a second root) carries the C20 half: what a leg can reach is its root,
positionally, and the clamp refuses the way out.

**Assert reachability, never a config field.** `ingress_rule` is read exactly
once here, out of a `reflect` REPLY, and only to assert that the posture the
system REPORTS is the posture it ENFORCES — the coherence bug measured on
2026-08-07, where a leg carrying `auth:"allow_all"` reflected `sealed: false`
and denied every inbound call. Everything else is an ATTEMPT.

Two behaviours are asserted as CORRECT and marked `xfail(strict=True)` because
the kernel does something else today; both are named at the bottom of this file.
"""

from __future__ import annotations

import pytest

from _engine import check, dehydrate, denial_side, denied, reach

SEAL = "edge sealed by policy"


async def _seed(h) -> None:
    """Materialize `data/note.txt` through the OPEN writable leg.

    Setup, not an assertion — but it goes through `reach()` like everything else,
    because the open leg working is the baseline the seals are measured against.
    Nothing else in this module creates a byte, so anything found in `data/`
    later came from here or from a leg that should not have written it.
    """
    r = await reach(h.kernel, "static_loader", "rw", _write("note.txt", "hello"))
    assert r.get("written") is True, f"the open leg must work first: {r!r}"


def _write(path: str, content: str) -> dict:
    return {"type": "write", "path": path, "content": content}


async def _names_in(h, leg: str) -> set[str]:
    """The file names `leg` can actually see in its root, right now."""
    r = await reach(h.kernel, "static_loader", leg, {"type": "list", "path": ""})
    assert "files" in r, f"{leg} could not list its own root: {r!r}"
    return {f["name"] for f in r["files"]}


# ─── the seal itself ────────────────────────────────────────────


async def test_a_sealed_leg_refuses_the_write_and_the_byte_never_reaches_the_disk(
    hydrate_scenario,
):
    """Two legs, one directory, opposite verdicts — and the refusal is checked for
    EFFECT, not just for shape.

    A denial that still wrote would be the worst possible failure: the caller
    handles an error while the side effect happened anyway. `rw` and `sealed_fs`
    are rooted at the same `data/`, so `rw`'s listing is an INDEPENDENT witness —
    it is not the sealed leg reporting on its own restraint.
    """
    h = await hydrate_scenario("sealed_by_default")
    await _seed(h)

    r = await reach(h.kernel, "static_loader", "sealed_fs", _write("sneak.txt", "x"))
    check("C23", denied(r), f"a sealed leg must refuse a write: {r!r}")
    check("C23", r.get("error") == SEAL, f"the refusal must say why: {r!r}")
    check(
        "C23",
        "ingress_rule=allow_all_ingress" in (r.get("hint") or ""),
        f"the locked door is a signpost — it must name the opening move: {r!r}",
    )

    seen = await _names_in(h, "rw")
    check(
        "C23",
        "sneak.txt" not in seen,
        f"the refused write LANDED anyway — rw sees {sorted(seen)} in the same root",
    )
    check("C23", "note.txt" in seen, f"the open leg's own write vanished: {seen!r}")


async def test_the_seal_denies_the_operator_too_because_it_gates_the_edge(
    hydrate_scenario,
):
    """The io seal is not an authorization about WHO is calling.

    The call gate has a root of trust: an operator send (`_current_sender` is
    None — the CLI, the cold bootstrap, this test driver) never reaches it. The
    io gate has no such notion. `file_bridge.handler` consults
    `gate_inbound(resolve_ingress(record), …)` without ever looking at the
    sender, so the same operator authority that can create and delete agents at
    will cannot read one byte through a sealed leg.

    That is the sharpest statement of sealed-by-default: the seal is a property
    of the EDGE, not a rule about callers, and it cannot be out-ranked.
    """
    h = await hydrate_scenario("sealed_by_default")
    await _seed(h)

    # No `sender_context` — this is an operator send, ungated by the CALL gate.
    r = (
        await h.kernel.send(
            [{"target": "sealed_fs", "payload": {"type": "list", "path": ""}}]
        )
    )[0]["result"]
    check("C23", denied(r), f"operator authority opened the io seal: {r!r}")
    check("C23", r.get("error") == SEAL, f"{r!r}")

    # The same operator send through the OPEN leg answers, so the denial above is
    # the seal and not some ambient failure of operator sends.
    ok = (
        await h.kernel.send([{"target": "rw", "payload": {"type": "list", "path": ""}}])
    )[0]["result"]
    check("C23", not denied(ok), f"the open leg must answer an operator: {ok!r}")


async def test_the_io_seal_and_the_call_gate_are_two_gates_and_only_one_names_a_side(
    hydrate_scenario,
):
    """Both refusals say `reason: unauthorized`, so `reason` alone cannot tell an
    operator which gate to go fix. `side` can, and its ABSENCE is meaningful.

    `static_loader → sealed_fs` passes the call gate — proven on the same edge by
    a `reflect` that answers — and is then refused by the seal, with no `side`.
    `rw → vault` is refused before it leaves the sender, with `side: egress`. The
    remedies are unrelated: one is an `ingress_rule` on the target's record, the
    other is a `can_call` grant from above, and the two hints say so.
    """
    h = await hydrate_scenario("sealed_by_default")

    call_gate_open = await reach(h.kernel, "static_loader", "sealed_fs", _reflect())
    check(
        "C23",
        not denied(call_gate_open),
        f"the call gate must be open on this edge for the claim to hold: "
        f"{call_gate_open!r}",
    )

    seal = await reach(h.kernel, "static_loader", "sealed_fs", {"type": "mkdir"})
    gate = await reach(h.kernel, "rw", "vault", {"type": "list", "path": ""})
    check("C23", denied(seal) and denied(gate), f"{seal!r} / {gate!r}")
    check(
        "C23",
        denial_side(seal) is None,
        f"an io-gate refusal has no `side` — it is not two-sided: {seal!r}",
    )
    check(
        "C23",
        denial_side(gate) == "egress",
        f"a call-gate refusal names the half that refused: {gate!r}",
    )
    check(
        "C23",
        seal.get("hint") != gate.get("hint"),
        "two different gates must teach two different remedies, or a caller "
        f"cannot act on either: {seal.get('hint')!r}",
    )


def _reflect() -> dict:
    return {"type": "reflect"}


async def test_a_sealed_leg_describes_itself_and_enforces_the_posture_it_reports(
    hydrate_scenario,
):
    """Discovery survives the seal, by design: `reflect` is in `file_bridge.UNGATED`
    so the locked door can explain itself. Everything the leg ADVERTISES is
    refused — the list of denied verbs is taken from the leg's own reply, so a
    verb added to the bundle is covered without editing this test.

    The posture is then cross-checked against behaviour. That is not a field
    check dressed up: it is the coherence bug measured on 2026-08-07, when
    `resolve_ingress` read only `ingress_rule` while `describe()` read
    `ingress_rule` else `auth` — a leg carrying `auth:"allow_all"` reflected
    `{sealed: false}` and denied every call. Fail-closed, so nothing leaked, but
    the posture an operator READS has to be the posture the gate ENFORCES.
    """
    h = await hydrate_scenario("sealed_by_default")

    sealed = await reach(h.kernel, "static_loader", "sealed_fs", _reflect())
    check("C23", sealed.get("id") == "sealed_fs", f"reflect must answer: {sealed!r}")
    check(
        "C23",
        sealed.get("sealed") is True and sealed.get("ingress_rule") == "deny_ingress",
        f"an absent rule must REPORT as the seal it is: {sealed!r}",
    )

    advertised = sorted(set(sealed.get("verbs") or {}) - {"reflect"})
    check("C23", bool(advertised), f"reflect must list the verbs: {sealed!r}")
    for verb in advertised:
        r = await reach(h.kernel, "static_loader", "sealed_fs", {"type": verb})
        check(
            "C23",
            denied(r) and r.get("error") == SEAL,
            f"{verb!r} is advertised on a sealed leg and was not refused: {r!r}",
        )

    # The other direction, so `sealed: true` is not simply what reflect always says.
    live = await reach(h.kernel, "static_loader", "rw", _reflect())
    check(
        "C23",
        live.get("sealed") is False and live.get("ingress_rule") == "allow_all_ingress",
        f"an open leg must report open: {live!r}",
    )
    check(
        "C23",
        not denied(await reach(h.kernel, "static_loader", "rw", {"type": "mkdir"})),
        "the leg reporting `sealed: false` must actually accept the verb",
    )


async def test_an_explicit_deny_ingress_refuses_exactly_like_an_absent_rule(
    hydrate_scenario,
):
    """`deny_ingress` written out and `ingress_rule` left off must be the SAME
    posture, byte for byte in the refusal.

    If the default were even slightly weaker than the explicit seal, "sealed by
    default" would be a slogan rather than a state, and an operator would have to
    write the rule down to actually get it.
    """
    h = await hydrate_scenario("sealed_by_default")
    probe = {"type": "read", "path": "note.txt"}
    implicit = await reach(h.kernel, "static_loader", "sealed_fs", probe)
    explicit = await reach(h.kernel, "static_loader", "denied_fs", probe)
    check(
        "C23",
        implicit == explicit,
        f"the default seal differs from the declared one: {implicit!r} vs {explicit!r}",
    )
    check("C23", denied(implicit), f"neither form denied: {implicit!r}")


async def test_an_undeclared_verb_errors_instead_of_quietly_doing_nothing(
    hydrate_scenario,
):
    """The bare C23 claim: a capability nobody declared answers with an ERROR, not
    with `None` and not with a success shape.

    Measured on a sealed leg deliberately — `file_bridge.handler` resolves the
    verb BEFORE it consults the gate, so an unimplemented verb is answered even
    behind the seal, and it is answered as a bundle error (no `reason`), not as a
    refusal. A caller can therefore tell "I asked for something that does not
    exist" from "I am not allowed", which is the difference between fixing the
    payload and fixing the posture.
    """
    h = await hydrate_scenario("sealed_by_default")
    r = await reach(h.kernel, "static_loader", "sealed_fs", {"type": "frobnicate"})
    check("C23", isinstance(r, dict) and bool(r.get("error")), f"silent no-op: {r!r}")
    check("C23", not denied(r), f"an unknown verb is not a gate denial: {r!r}")
    check(
        "C23",
        "frobnicate" in r["error"],
        f"the error must name what was not understood: {r!r}",
    )


# ─── opening it is a conscious act ──────────────────────────────


async def test_opening_the_seal_takes_an_operator_write_and_the_same_probe_then_passes(
    hydrate_scenario,
):
    """The whole scenario in one test: denied → an agent cannot open it → still
    denied → an operator writes the record → the SAME probe answers.

    The peer attempt is the interesting half. `static_loader` is the leg's
    PARENT and holds `{$children: "*"}`, which reaches every verb its children
    accept — except a PRIVILEGED one, because a wildcard never covers
    `update_agent` (`kernel/_authz.py:73-77`). So "talk to your own subtree"
    cannot silently become "unseal your own subtree", and the leg stays shut.

    The re-probe after the refusal is the point of doing it this way: the effect
    of a refused mutation is measured by ATTEMPTING the operation again, never by
    reading back the field it declined to write.
    """
    h = await hydrate_scenario("sealed_by_default")
    await _seed(h)
    probe = {"type": "read", "path": "note.txt"}

    check(
        "C23",
        denied(await reach(h.kernel, "static_loader", "sealed_fs", probe)),
        "the leg must start sealed",
    )

    peer = await reach(
        h.kernel,
        "static_loader",
        "sealed_fs",
        {
            "type": "update_agent",
            "id": "sealed_fs",
            "ingress_rule": "allow_all_ingress",
        },
    )
    check("C23", denied(peer), f"a parent unsealed its child: {peer!r}")
    check(
        "C23",
        denial_side(peer) == "egress",
        f"a wildcard must not cover update_agent: {peer!r}",
    )
    check(
        "C23",
        denied(await reach(h.kernel, "static_loader", "sealed_fs", probe)),
        "the refused update opened the leg anyway",
    )

    opened = (
        await h.kernel.send(
            [
                {
                    "target": h.kernel.id,
                    "payload": {
                        "type": "update_agent",
                        "id": "sealed_fs",
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]
    check(
        "C23", opened.get("updated") is True, f"the operator write failed: {opened!r}"
    )

    after = await reach(h.kernel, "static_loader", "sealed_fs", probe)
    check(
        "C23",
        after.get("content") == "hello",
        f"the same probe must now answer — one record write is the whole "
        f"difference: {after!r}",
    )


async def test_a_near_miss_rule_name_is_refused_at_the_write_and_leaves_it_sealed(
    hydrate_scenario,
):
    """Rule names are MANIFOLD-SCOPED, and the pre-rename bare stem `allow_all` is
    now a name no ingress registry has.

    Two things must hold and both are behavioural. The write is refused where the
    typo is (`validate_meta`), rather than accepted and turned into
    `DenyInbound(unknown='allow_all')` three calls later — the failure that
    prompted the scoping, when `auth: "silent"` (a legitimate EGRESS policy)
    silently SEALED an ingress leg. And the refusal must be FOLLOWABLE: every
    value it names as legal has to be a value that same write would accept, or
    the operator is sent in a circle. `sealed_proc` fails that second half today
    — see the xfail at the bottom of this file.
    """
    h = await hydrate_scenario("sealed_by_default")
    await _seed(h)

    bad = (
        await h.kernel.send(
            [
                {
                    "target": h.kernel.id,
                    "payload": {
                        "type": "update_agent",
                        "id": "sealed_fs",
                        "ingress_rule": "allow_all",
                    },
                }
            ]
        )
    )[0]["result"]
    check(
        "C23", bad.get("reason") == "invalid_meta", f"a name nobody implements: {bad!r}"
    )
    check(
        "C23",
        denied(await reach(h.kernel, "static_loader", "sealed_fs", {"type": "list"})),
        "a refused near-miss half-opened the leg",
    )

    for name in _legal_values(bad.get("error") or ""):
        r = (
            await h.kernel.send(
                [
                    {
                        "target": h.kernel.id,
                        "payload": {
                            "type": "update_agent",
                            "id": "sealed_fs",
                            "ingress_rule": name,
                        },
                    }
                ]
            )
        )[0]["result"]
        check(
            "C23",
            r.get("updated") is True,
            f"the refusal named {name!r} as legal and the same write refused it "
            f"— following the hint loops forever: {r!r}",
        )


def _legal_values(message: str) -> list[str]:
    """The names a write-time refusal offered as the way forward."""
    _, _, tail = message.partition("legal values:")
    return [n.strip() for n in tail.split(",") if n.strip()]


# ─── the other three C23 forms ──────────────────────────────────


async def test_a_readonly_leg_reads_and_refuses_to_write_without_calling_it_a_denial(
    hydrate_scenario,
):
    """`readonly` is a capability the RECORD does not grant, and the bundle refuses
    it — a different animal from the gate, and it must not masquerade as one.

    So the reply carries an `error` and no `reason`: nothing about the posture
    would change if the operator opened the ingress further, and a caller that
    treated this as an authorization problem would go and open the wrong thing.
    The witness is `rw`, rooted at the same directory: the byte never landed.
    """
    h = await hydrate_scenario("sealed_by_default")
    await _seed(h)

    got = await reach(
        h.kernel, "static_loader", "ro", {"type": "read", "path": "note.txt"}
    )
    check("C23", got.get("content") == "hello", f"readonly still reads: {got!r}")

    refused = await reach(h.kernel, "static_loader", "ro", _write("ro.txt", "x"))
    check("C23", refused.get("error") == "agent is readonly", f"{refused!r}")
    check(
        "C23",
        not denied(refused),
        f"a readonly refusal is not an authorization verdict: {refused!r}",
    )
    seen = await _names_in(h, "rw")
    check("C23", "ro.txt" not in seen, f"the readonly write landed: {sorted(seen)}")


async def test_two_legs_rooted_in_different_directories_cannot_reach_across(
    hydrate_scenario,
):
    """What a leg can reach is decided by its `root`, positionally — there is no
    naming convention and no shared namespace to negotiate.

    Both legs here are `allow_all_ingress`, so the isolation is not the gate: it
    is the clamp in `file_bridge.fs`, the one module in the kernel that calls
    `open()`. The way out is refused by path, not by policy, so an open leg is
    still bounded.
    """
    h = await hydrate_scenario("sealed_by_default")
    await _seed(h)

    w = await reach(h.kernel, "static_loader", "vault", _write("secret.txt", "s"))
    check("C20", w.get("written") is True, f"the second leg must work: {w!r}")

    check("C20", await _names_in(h, "vault") == {"secret.txt"}, "vault sees data/")
    check("C20", "secret.txt" not in await _names_in(h, "rw"), "rw sees vault/")

    escape = await reach(
        h.kernel, "static_loader", "vault", {"type": "read", "path": "../data/note.txt"}
    )
    check(
        "C20",
        "escapes root" in (escape.get("error") or ""),
        f"an open leg still cannot walk out of its root: {escape!r}",
    )
    check("C20", "content" not in escape, f"the escape returned bytes: {escape!r}")


async def test_persistence_nobody_wired_fails_loud_and_keeps_nothing_in_ram(
    hydrate_scenario,
):
    """The capability that is missing by POSITION rather than by posture.

    `_find_store` scans only the DIRECT CHILDREN of the root loader, and every
    manifest entry is a child of `static_loader` — a grandchild — so this config
    declares no store at all (configs/README.md, known gap #1). `yaml_state.set`
    must then say so and lose the value, because a RAM fallback would be a second
    truth: the caller would see `{set: true}`, read it back within the process,
    and lose it at the next boot with no error anywhere.

    Read stays LENIENT by design (a never-written store reads empty), so the
    absence is asserted as `value: null` — which is exactly what a caller who
    ignored the write error would find.
    """
    h = await hydrate_scenario("sealed_by_default")

    w = await reach(
        h.kernel, "static_loader", "memory", {"type": "set", "key": "k", "value": 1}
    )
    check("C23", "no store wired" in (w.get("error") or ""), f"silent RAM: {w!r}")
    check("C23", w.get("set") is not True, f"a failed write reported success: {w!r}")

    back = await reach(
        h.kernel, "static_loader", "memory", {"type": "read", "key": "k"}
    )
    check("C23", back.get("value") is None, f"the value survived in RAM: {back!r}")
    keys = await reach(h.kernel, "static_loader", "memory", {"type": "keys"})
    check("C23", keys.get("keys") == [], f"the doc is not empty: {keys!r}")

    # The running system DISCLOSES the hole rather than implying a store: the root
    # reflect's `persistence.provider` is the same fact the failfast reported.
    d = await dehydrate(h.kernel, disk=True)
    check(
        "C23",
        d["persistence_provider"] is None,
        f"a provider is reported while writes failfast: {d['persistence_provider']!r}",
    )
    sidecars = [p for p in d["disk_files"] if p.endswith("state.yaml")]
    check("C23", sidecars == [], f"the failed write left a sidecar: {sidecars!r}")


# ─── measured defects, asserted as they SHOULD behave ───────────


@pytest.mark.xfail(
    strict=True,
    reason=(
        "MEASURED DEFECT. An io leg can open its OWN seal: with sender_context"
        "('sealed_fs'), `update_agent id=sealed_fs ingress_rule=allow_all_ingress`"
        " returns {'updated': True} and the next call through the leg answers."
        " The call gate's self-raise guard (kernel/_agent.py:_check_conferral)"
        " fires only on `can_call`/`callable_by`, and `ingress_rule` — the io"
        " manifold's `callable_by`, deciding who may reach in — is not covered,"
        " while egress lets it through because acting on your own id is baseline."
        " No shipped io bundle originates sends as itself, so nothing exploits it"
        " today; the asymmetry is the finding. Un-xfail if the kernel is changed,"
        " or delete this test if self-unsealing is ruled to be by design."
    ),
)
async def test_a_leg_may_not_open_its_own_seal(hydrate_scenario):
    """Authority flows from ABOVE only — the kernel's own words
    (`kernel/_authz.py`): "An agent can NEVER raise its own authority."

    `ingress_rule` is what `callable_by` is, for the io edge: it decides who may
    reach in. Editing your own `callable_by` is refused with `side: self-grant`.
    Editing your own `ingress_rule` should be the same refusal, or the seal is
    only sealed against everybody except the one agent behind it.
    """
    h = await hydrate_scenario("sealed_by_default")
    await _seed(h)

    r = await reach(
        h.kernel,
        "sealed_fs",
        "sealed_fs",
        {
            "type": "update_agent",
            "id": "sealed_fs",
            "ingress_rule": "allow_all_ingress",
        },
    )
    check("C23", denied(r), f"a sealed leg unsealed itself: {r!r}")
    check("C23", denial_side(r) == "self-grant", f"{r!r}")
    check(
        "C23",
        denied(await reach(h.kernel, "static_loader", "sealed_fs", {"type": "list"})),
        "the self-unseal took effect",
    )


@pytest.mark.xfail(
    strict=True,
    reason=(
        "MEASURED DEFECT. `exec_bridge.validate_meta` (tools.py:596-602) still"
        ' names the PRE-RENAME bare stems: "legal values: allow_all,'
        ' deny_inbound, password". All three are absent from'
        " io_bridge.ingress_rules.REGISTRY (allow_all_ingress / deny_ingress /"
        " password_ingress), so the same check refuses every value its own"
        " refusal recommends — an operator following the hint gets the identical"
        " message back, forever. file_bridge/tools.py:436 names the scoped set"
        " and passes this test. One-line fix in exec_bridge."
    ),
)
async def test_a_sealed_process_edge_names_a_rule_that_would_actually_open_it(
    hydrate_scenario,
):
    """Discovery-through-denial is the design: a refusal that does not say what
    WOULD work only teaches that something is wrong. The bundle's own docstring
    records why — gemma-4-12B tried `ingress_rule="deny_all"`, the refusal named
    the legal values, and it used a working name on its very next turn.

    So the contract is not "name some values", it is "name values that work": a
    write refused for an unknown rule must offer names this same write accepts.
    """
    h = await hydrate_scenario("sealed_by_default")

    bad = (
        await h.kernel.send(
            [
                {
                    "target": h.kernel.id,
                    "payload": {
                        "type": "update_agent",
                        "id": "sealed_proc",
                        "ingress_rule": "allow_all",
                    },
                }
            ]
        )
    )[0]["result"]
    check(
        "C23",
        bad.get("reason") == "invalid_meta",
        f"an unknown rule was accepted: {bad!r}",
    )
    offered = _legal_values(bad.get("error") or "")
    check("C23", bool(offered), f"the refusal names no way forward: {bad!r}")
    for name in offered:
        r = (
            await h.kernel.send(
                [
                    {
                        "target": h.kernel.id,
                        "payload": {
                            "type": "update_agent",
                            "id": "sealed_proc",
                            "ingress_rule": name,
                        },
                    }
                ]
            )
        )[0]["result"]
        check(
            "C23",
            r.get("updated") is True,
            f"offered {name!r} as legal, then refused it: {r!r}",
        )
    check(
        "C23",
        not denied(await reach(h.kernel, "static_loader", "sealed_proc", _reflect())),
        "reflect must survive on the process edge too",
    )
