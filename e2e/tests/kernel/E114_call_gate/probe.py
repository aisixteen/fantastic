"""kernel/call_gate — the two-sided deny-all CALL GATE, over the wire. NO MODEL.

An AGENT send passes only when the SENDER's `can_call` grants `(target, verb)`
(egress) AND the TARGET's `callable_by` grants `(sender, verb)` (ingress); a
denial is `reason: unauthorized` naming the `side`, egress first. On the PACKAGED
host of `RUNTIME` (python | rust | swift), driven over `web_rest` — the agents
under test are `user_probe` agents whose `send` dispatches AS THEMSELVES, so the
sender is a real agent, not the REST leg:

  1. `$self` is the baseline; neither half → egress; the caller's half alone →
     ingress; both halves → the edge exists; it is one-way;
  2. VERB-LEVEL grants: `["reflect"]` admits reflect only — another verb dies on
     whichever half lacks it; `"*"` covers every verb;
  3. TOKENS resolve relative to the holder: `$parent` (only the holder's own
     parent), `$subtree` (every agent below the holder, never a sibling);
  4. the REST leg itself is gated: a target outside its `can_call` → egress;
  5. an agent cannot raise its own authority: `update_agent` widening its own
     `can_call` is refused and the record is unchanged;
  6. a grant map's shape is checked at the write — `invalid_meta`;
  7. `delete_agent` prunes the deleted ids (the whole subtree) from every other
     agent's grants, so a re-created id does not inherit a stale edge.
"""

from __future__ import annotations

import os
import shutil
import sys
import tempfile
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
USER = "user_probe.tools"
SELF = {"$self": "*"}
FROM_ABOVE = "grants come from above"


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-call-gate-"))
    hk = HostKernel(spec, work, reach=["a", "b", "c"]).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def rec(aid: str) -> dict:
        la = hk.send(root, {"type": "list_agents"})
        return next((a for a in la.get("agents") or [] if a.get("id") == aid), {})

    def setg(aid: str, field: str, grants: dict) -> None:
        """The operator (the REST leg, holding update_agent on the root) writes a
        WHOLE grant field — update_agent replaces it."""
        r = hk.send(root, {"type": "update_agent", "id": aid, field: grants})
        if r.get("updated") is not True:
            bad.append(f"setup: update_agent {aid}.{field}={grants}: {str(r)[:200]}")
            print(f"  BAD setup {aid}.{field}: {str(r)[:200]}", flush=True)

    def via(src: str, target: str, payload: dict) -> dict:
        """`src` sends `payload` to `target` AS ITSELF; the onward result."""
        r = hk.send(src, {"type": "send", "target": target, "payload": payload})
        if "sent" not in r:
            return {"error": f"relay {src} refused: {str(r)[:200]}", "relay": True}
        return r.get("result") or {}

    def refused(r: dict, side: str | None) -> bool:
        return (
            r.get("reason") == "unauthorized"
            and (side is None or r.get("side") == side)
            and not r.get("relay")
        )

    def allowed(r: dict) -> bool:
        return "error" not in r and "reason" not in r

    try:
        for aid, parent, cb in (
            ("a", "rest", {**SELF, "$parent": "*"}),
            ("b", "rest", {**SELF, "$parent": "*"}),
            # c is a's child; the REST leg (its grandparent) drives it by name
            ("c", "a", {**SELF, "rest": ["send"]}),
        ):
            r = hk.create(parent_id=parent, handler_module=USER, id=aid, callable_by=cb)
            need(r.get("id") == aid, f"create {aid} under {parent}: {str(r)[:160]}")
        # an agent OUTSIDE the REST leg's reach: under the root, never declared
        r = hk.send(root, {"type": "create_agent", "parent_id": root, "id": "outsider",
                           "handler_module": USER, "callable_by": {**SELF, "rest": "*"}})
        need(r.get("id") == "outsider", f"create outsider under the root: {str(r)[:160]}")

        # 1 · the two halves
        r = via("a", "a", {"type": "reflect"})
        need(allowed(r) and r.get("id") == "a", f"$self baseline: a→a reflect passes: {str(r)[:120]}")
        r = via("a", "b", {"type": "reflect"})
        need(refused(r, "egress"), f"neither half → refused, side egress: {str(r)[:200]}")
        need(FROM_ABOVE in (r.get("hint") or ""), f"the denial carries the from-above hint: {r.get('hint')!r}")
        setg("a", "can_call", {**SELF, "b": ["reflect"]})
        r = via("a", "b", {"type": "reflect"})
        need(refused(r, "ingress"), f"caller's half only → refused, side ingress: {str(r)[:200]}")
        setg("b", "callable_by", {**SELF, "$parent": "*", "a": ["reflect"]})
        r = via("a", "b", {"type": "reflect"})
        need(allowed(r) and r.get("id") == "b", f"both halves → a→b reflect passes: {str(r)[:120]}")
        r = via("b", "a", {"type": "reflect"})
        need(refused(r, "egress"), f"the edge is one-way: b→a refused, side egress: {str(r)[:200]}")

        # 2 · verb-level grants
        r = via("a", "b", {"type": "inquiries"})
        need(refused(r, "egress"), f"verb not in a's can_call list → egress: {str(r)[:200]}")
        setg("a", "can_call", {**SELF, "b": "*"})
        r = via("a", "b", {"type": "inquiries"})
        need(refused(r, "ingress"), f"a holds '*', b accepts reflect only → ingress: {str(r)[:200]}")
        r = via("a", "b", {"type": "reflect"})
        need(allowed(r), f"the verb both halves name still crosses: {str(r)[:120]}")
        setg("b", "callable_by", {**SELF, "$parent": "*", "a": "*"})
        r = via("a", "b", {"type": "inquiries"})
        need(allowed(r) and "inquiries" in r, f"'*' on both halves covers every verb: {str(r)[:120]}")

        # 3 · tokens, relative to the holder
        setg("a", "can_call", {**SELF, "b": "*", "$subtree": ["reflect"]})
        setg("c", "callable_by", {**SELF, "rest": ["send"], "$parent": ["reflect"]})
        r = via("a", "c", {"type": "reflect"})
        need(allowed(r) and r.get("id") == "c",
             f"a.$subtree + c.$parent → a→c reflect passes: {str(r)[:120]}")
        r = via("a", "c", {"type": "inquiries"})
        need(refused(r, "egress"), f"$subtree granted reflect only → inquiries egress: {str(r)[:200]}")
        setg("b", "can_call", {**SELF, "c": ["reflect"]})
        r = via("b", "c", {"type": "reflect"})
        need(refused(r, "ingress"),
             f"c.$parent admits its parent only — b→c refused, side ingress: {str(r)[:200]}")
        setg("b", "can_call", {**SELF, "$subtree": "*"})
        r = via("b", "a", {"type": "reflect"})
        need(refused(r, "egress"), f"b.$subtree never reaches a sibling (a) → egress: {str(r)[:200]}")
        setg("c", "can_call", {**SELF, "$parent": ["inquiries"]})
        setg("a", "callable_by", {**SELF, "$parent": "*", "$subtree": ["inquiries"]})
        r = via("c", "a", {"type": "inquiries"})
        need(allowed(r) and "inquiries" in r,
             f"c.$parent + a.$subtree → c→a inquiries passes: {str(r)[:120]}")
        setg("b", "can_call", {**SELF, "a": ["inquiries"], "c": ["reflect"]})
        r = via("b", "a", {"type": "inquiries"})
        need(refused(r, "ingress"),
             f"a.$subtree does not admit a sibling (b) → ingress: {str(r)[:200]}")

        # 4 · the REST leg is an agent too
        r = hk.send("outsider", {"type": "reflect"})
        need(refused(r, "egress"), f"REST → an undeclared agent → refused, side egress: {str(r)[:200]}")

        # 5 · no self-raise
        before = rec("a").get("can_call")
        r = via("a", root, {"type": "update_agent", "id": "a",
                            "can_call": {**SELF, "outsider": "*"}})
        need(refused(r, None), f"a's update_agent on itself (via the root) is refused: {str(r)[:200]}")
        r2 = via("a", "a", {"type": "update_agent", "id": "a",
                            "can_call": {**SELF, "outsider": "*"}})
        need("error" in r2 and r2.get("updated") is not True,
             f"a's update_agent sent to itself is refused: {str(r2)[:200]}")
        need(rec("a").get("can_call") == before, f"a's can_call is unchanged: {rec('a').get('can_call')!r}")
        r = via("a", "outsider", {"type": "reflect"})
        need(refused(r, "egress"), f"and the edge it tried to open stays shut: {str(r)[:160]}")

        # 6 · grant shape, checked at the write
        for field, grants, what in (
            ("can_call", {"$bogus": "*"}, "an unknown $token"),
            ("callable_by", {"g1|x": "*"}, "a peer address in callable_by"),
            ("can_call", {"b": 5}, "a value that is neither '*' nor a verb list"),
        ):
            r = hk.send(root, {"type": "update_agent", "id": "b", field: grants})
            need(r.get("reason") == "invalid_meta", f"{field} {what} → invalid_meta: {str(r)[:200]}")
        r = hk.send(root, {"type": "update_agent", "id": "b",
                           "can_call": {**SELF, "a": ["inquiries"], "c": ["reflect"], "g1|x": "*"}})
        need(r.get("updated") is True, f"can_call takes a <guid>|<agent> key: {str(r)[:160]}")

        # 7 · delete prunes grants
        need("b" in (rec("a").get("can_call") or {}), "before: a.can_call names b")
        r = hk.send(root, {"type": "delete_agent", "id": "b"})
        need(r.get("deleted") is True, f"delete b: {str(r)[:160]}")
        a_can = rec("a").get("can_call") or {}
        need("b" not in a_can, f"b is pruned from a.can_call: {a_can!r}")
        r = hk.create(parent_id="rest", handler_module=USER, id="b",
                      callable_by={**SELF, "$parent": "*", "a": "*"})
        need(r.get("id") == "b", f"re-create b: {str(r)[:120]}")
        r = via("a", "b", {"type": "reflect"})
        need(refused(r, "egress"), f"the re-created b inherits no stale edge from a: {str(r)[:200]}")
        setg("b", "can_call", {**SELF, "a": ["reflect"], "c": ["reflect"]})
        r = hk.send(root, {"type": "delete_agent", "id": "a"})
        need(r.get("deleted") is True, f"delete a (and its child c): {str(r)[:160]}")
        need(not rec("c"), "c went with its parent")
        b_can = rec("b").get("can_call") or {}
        need("a" not in b_can and "c" not in b_can,
             f"the whole deleted subtree is pruned from b.can_call: {b_can!r}")
        o_cb = rec("outsider").get("callable_by") or {}
        need(o_cb == {**SELF, "rest": "*"}, f"an unrelated grant map is untouched: {o_cb!r}")
    finally:
        hk.close()
        shutil.rmtree(work, ignore_errors=True)

    verdict(
        not bad,
        f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""),
    )
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
