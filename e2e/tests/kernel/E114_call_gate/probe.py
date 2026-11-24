"""kernel/call_gate — the CALL GATE on `can` lines, over the wire. NO MODEL.

A call passes only when the CALLER's own `can` holds a line covering
`(target, verb)` — or the target is in the caller's own tree (position, `call`
only) — AND every ancestor of the caller whose tree does not hold the target
allows it too (the cap; the root never caps). The target keeps no list: it can
neither widen nor block a call. Every denial is the UNKNOWN ANSWER, byte for byte
the answer for a missing id (no side, no hint, no ids). On the PACKAGED rust
host, driven over `web_rest` — the agents under test are `user_probe` agents
whose `send` dispatches AS THEMSELVES, so the caller is a real agent, not the
REST leg:

  1. one line on the caller: none → unknown, the same as a missing id; a line →
     passes, the target's record unchanged; one-way;
  2. LEVELS: `call: [reflect]` admits reflect only; `call: [*]` every verb;
     `update` includes `call` on every verb;
  3. POSITION (own tree, no line), `$parent` (the parent needs a written line),
     siblings need a line, a line reaches the target's tree, THE WRITER SUBSET
     (`rest` hands out only what its written lines hold), and THE CAP (an
     ancestor that does not hold the target cuts the call);
  4. the REST leg is an agent too: a target it holds no covering line on → the
     unknown answer, a create on the root it holds no `create` line on → unknown;
  5. NEVER UPWARD: an agent cannot write its own `can`, nor its parent's record;
  6. a line's shape is checked at the write (`invalid_meta`); a line naming a
     missing id, written by an agent, is the unknown answer for that id;
  7. IDS ARE REUSABLE: a line survives its target's delete, a new line on an
     absent id is refused, a re-created id is reached by the surviving line with
     no write, and a delete takes the whole subtree.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.lines import ALL, call, is_unknown
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
USER = "user_probe.tools"
#: The OUTSIDER: the seeded record store at the root, outside `rest`'s tree.
#: `rest` holds only `reflect` on it (through its line on the root).
OUTSIDER = "records"
MISSING = "no_such_id"


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-call-gate-"))
    hk = HostKernel(spec, work).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def rec(aid: str) -> dict:
        """`aid`'s record as `rest` sees it: whole inside `rest`'s tree; `{}` on
        the unknown answer."""
        r = hk.send(aid, {"type": "reflect"})
        return {} if not isinstance(r, dict) or is_unknown(r, aid) else r

    def write_can(aid: str, can: dict) -> dict:
        """`rest` writes `aid`'s WHOLE `can` (update_agent replaces the field)."""
        return hk.send(root, {"type": "update_agent", "id": aid, "can": can})

    def setg(aid: str, can: dict) -> None:
        r = write_can(aid, can)
        if r.get("updated") is not True:
            bad.append(f"setup: update_agent {aid}.can={can}: {str(r)[:200]}")
            print(f"  BAD setup {aid}.can: {str(r)[:200]}", flush=True)

    def via(src: str, target: str, payload: dict) -> dict:
        """`src` sends `payload` to `target` AS ITSELF; the onward result."""
        r = hk.send(src, {"type": "send", "target": target, "payload": payload})
        if "sent" not in r:
            return {"error": f"relay {src} refused: {str(r)[:200]}", "relay": True}
        return r.get("result") or {}

    def refused(r: dict, target: str) -> bool:
        return is_unknown(r, target) and not r.get("relay")

    def allowed(r: dict) -> bool:
        return "error" not in r and "reason" not in r

    def same_as_missing(src: str, target: str, payload: dict) -> None:
        got = via(src, target, payload)
        miss = via(src, MISSING, payload)
        if isinstance(miss.get("error"), str):
            miss = {**miss, "error": miss["error"].replace(repr(MISSING), repr(target))}
        need(got == miss, f"{src}→{target} denial is byte-identical to a missing id: {got} vs {miss}")

    def dumps(r: dict) -> str:
        return json.dumps(r, sort_keys=True)

    try:
        for aid, parent in (("a", "rest"), ("b", "rest"), ("c", "a")):
            r = hk.create(parent_id=parent, handler_module=USER, id=aid)
            need(r.get("id") == aid, f"create {aid} under {parent}: {str(r)[:160]}")
            need(not rec(aid).get("can"), f"a new agent holds no lines: {aid}.can={rec(aid).get('can')!r}")

        # 1 · one line, on the caller
        r = via("a", "a", {"type": "reflect"})
        need(allowed(r) and r.get("id") == "a", f"own tree: a→a reflect passes: {str(r)[:120]}")
        r = via("a", "b", {"type": "reflect"})
        need(refused(r, "b"), f"no line → the unknown answer: {str(r)[:200]}")
        same_as_missing("a", "b", {"type": "reflect"})
        b_before = dumps(rec("b"))
        setg("a", {"b": call("reflect")})
        r = via("a", "b", {"type": "reflect"})
        need(allowed(r) and r.get("id") == "b", f"a's line → a→b reflect passes: {str(r)[:120]}")
        need(dumps(rec("b")) == b_before,
             "the target's record is byte-equal before and after (it can neither widen nor block)")
        r = via("b", "a", {"type": "reflect"})
        need(refused(r, "a"), f"the line is one-way: b→a refused: {str(r)[:200]}")

        # 2 · levels
        r = via("a", "b", {"type": "inquiries"})
        need(refused(r, "b"), f"call: [reflect] does not cover inquiries: {str(r)[:200]}")
        setg("a", {"b": ALL})
        r = via("a", "b", {"type": "inquiries"})
        need(allowed(r) and "inquiries" in r, f"call: [*] covers every verb: {str(r)[:120]}")
        setg("a", {"b": {"update": []}})
        r = via("a", "b", {"type": "inquiries"})
        need(allowed(r) and "inquiries" in r, f"update includes call on every verb: {str(r)[:120]}")

        # 3 · position, $parent, siblings, the cap
        setg("a", {})
        r = via("a", "c", {"type": "inquiries"})
        need(allowed(r) and "inquiries" in r, f"position: a→c (its child) passes with no line: {str(r)[:120]}")
        r = via("c", "a", {"type": "inquiries"})
        need(refused(r, "a"), f"the parent is not reached by position: c→a refused: {str(r)[:200]}")
        setg("c", {"$parent": call("inquiries")})
        r = via("c", "a", {"type": "inquiries"})
        need(allowed(r) and "inquiries" in r, f"c's $parent line → c→a inquiries passes: {str(r)[:120]}")
        r = via("c", "a", {"type": "reflect"})
        need(allowed(r) and r.get("id") == "a", f"reflect passes on any hold: c→a reflect: {str(r)[:120]}")
        r = via("b", "c", {"type": "reflect"})
        need(refused(r, "c"), f"a sibling's child is not reached: b→c refused: {str(r)[:200]}")
        setg("b", {"a": call("reflect")})
        r = via("b", "c", {"type": "reflect"})
        need(allowed(r) and r.get("id") == "c", f"a line reaches the target's tree: b→c reflect: {str(r)[:120]}")
        r = write_can("c", {OUTSIDER: ALL})
        need(r.get("reason") == "not_allowed" and r.get("arg") == "can",
             f"the writer subset: rest holds only reflect on {OUTSIDER}, so it cannot hand out "
             f"call: [*] there: {str(r)[:200]}")
        r = write_can("c", {OUTSIDER: call("reflect")})
        need(r.get("updated") is True,
             f"rest hands c reflect on {OUTSIDER} (it holds it through the root line): {str(r)[:200]}")
        r = via("c", OUTSIDER, {"type": "reflect"})
        need(refused(r, OUTSIDER), f"the cap: a holds no line on {OUTSIDER}, so c's call is cut: {str(r)[:200]}")
        setg("a", {OUTSIDER: call("reflect")})
        r = via("c", OUTSIDER, {"type": "reflect"})
        need(allowed(r) and r.get("id") == OUTSIDER, f"once a holds it too, c→{OUTSIDER} passes: {str(r)[:120]}")

        # 4 · the REST leg is an agent too
        r = hk.send(OUTSIDER, {"type": "list"})
        need(is_unknown(r, OUTSIDER), f"REST → {OUTSIDER} list (reflect line only) → unknown: {str(r)[:200]}")
        r = hk.send("kernel", {"type": "create_agent", "parent_id": root, "id": "x",
                               "handler_module": USER})
        need(is_unknown(r, root), f"REST creates on the root with no create line there → unknown: {str(r)[:200]}")

        # 5 · never upward
        before = rec("a").get("can")
        r = via("a", "kernel", {"type": "update_agent", "id": "a", "can": {OUTSIDER: ALL}})
        need(refused(r, "a"), f"a's update_agent of its own can (via the root) → unknown: {str(r)[:200]}")
        r2 = via("a", "a", {"type": "update_agent", "id": "a", "can": {OUTSIDER: ALL}})
        need(r2.get("reason") == "not_here" and r2.get("updated") is not True,
             f"a's update_agent sent to itself is refused not_here: {str(r2)[:200]}")
        need(rec("a").get("can") == before, f"a's can is unchanged: {rec('a').get('can')!r}")
        r = via("a", OUTSIDER, {"type": "list"})
        need(refused(r, OUTSIDER), f"and the line it tried to write stays absent: {str(r)[:160]}")
        r = via("a", "kernel", {"type": "update_agent", "id": "rest", "description": "x"})
        need(refused(r, "rest"), f"a cannot write its parent's record: {str(r)[:200]}")

        # 6 · a line's shape, checked at the write
        # a line the verb's schema cannot parse is `bad_arg` (the argument check);
        # a well-formed line the model does not allow is `invalid_meta`
        for can, what, text, reason in (
            ({"$bogus": ALL}, "an unknown $ key", None, "invalid_meta"),
            ({"*": ALL}, "a wildcard target", None, "invalid_meta"),
            ({"b2": 5}, "a level that is not a map", None, "bad_arg"),
            ({"a": {"call": "x"}}, "a call level that is not a list", None, "bad_arg"),
            ({"a": {"call": ["x"], "update": []}}, "two levels on one target", None, "bad_arg"),
            ({"$parent": {"update": []}}, "$parent at a level other than call", None, "invalid_meta"),
            ({"g1|x": ALL}, "a peer target", "peer target", "invalid_meta"),
        ):
            before_b = rec("b").get("can")
            r = write_can("b", can)
            need(r.get("reason") == reason and (text is None or text in str(r.get("error")))
                 and rec("b").get("can") == before_b,
                 f"can {what} → {reason}, b's can unchanged: {str(r)[:200]}")
        r = write_can("b", {"nobody_here": ALL})
        need(is_unknown(r, "nobody_here"),
             f"a line naming a missing id, written by an agent → unknown for that id: {str(r)[:200]}")

        # 7 · ids are reusable
        setg("a", {"b": call("reflect")})
        r = hk.send(root, {"type": "delete_agent", "id": "b"})
        need(r.get("deleted") is True, f"delete b: {str(r)[:160]}")
        need("b" in (rec("a").get("can") or {}), f"a's line on b survives b's delete: {rec('a').get('can')!r}")
        r = write_can("c", {"b": ALL})
        need(is_unknown(r, "b"), f"a new line on the absent b is refused, unknown: {str(r)[:200]}")
        r = hk.create(parent_id="rest", handler_module=USER, id="b")
        need(r.get("id") == "b", f"re-create b (no grant): {str(r)[:120]}")
        r = via("a", "b", {"type": "reflect"})
        need(allowed(r) and r.get("id") == "b",
             f"the re-created b is reached by a's surviving line, with no write: {str(r)[:120]}")
        r = hk.send(root, {"type": "delete_agent", "id": "a"})
        need(r.get("deleted") is True, f"delete a (and its child c): {str(r)[:160]}")
        need(rec("c") == {}, "c went with its parent")
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
