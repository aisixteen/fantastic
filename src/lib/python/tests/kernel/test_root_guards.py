"""Guards on the ROOT and on the record fields that name substrate wiring.

- `delete_agent id=<root>` must not cascade the whole tree and answer
  `{"deleted": true}`, leaving a process with no control surface.
- `update_agent` must not silently DROP `handler_module` and the reserved keys
  (`root_path`/`ctx`/`parent`) and still answer success, nor write the reserved
  keys into the record, which the loader would refuse on the next boot.
- A verb a bare agent cannot answer must not come back as a reason-less
  `{error: …}` that `_apply_reflect_flags` decorates with `kernel_verbs`/`tree`/
  `more` — a refusal wearing a reflect's clothes.
"""

from __future__ import annotations


async def _mk(kernel, agent_id, **meta):
    payload = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "id": agent_id,
        "handler_module": "file_bridge.tools",
        "allow": ["."],
        "ingress_rule": "allow_all_ingress",
        **meta,
    }
    rec = (await kernel.send([{"target": kernel.id, "payload": payload}]))[0]["result"]
    assert rec.get("id") == agent_id, rec
    return agent_id


async def _kernel_call(kernel, **payload):
    return (await kernel.send([{"target": "kernel", "payload": payload}]))[0]["result"]


# twin: delete_agent_refuses_the_root_with_python_text
async def test_delete_agent_refuses_the_root(seeded_kernel):
    child = await _mk(seeded_kernel, "rg_child")
    r = await _kernel_call(seeded_kernel, type="delete_agent", id=seeded_kernel.id)
    assert r.get("reason") == "bad_arg" and r.get("arg") == "id", r
    assert r.get("deleted") is False, r
    # The tree is intact.
    assert seeded_kernel.ctx.agents.get(seeded_kernel.id) is not None
    assert seeded_kernel.ctx.agents.get(child) is not None


async def test_update_agent_refuses_handler_module_and_reserved_keys(seeded_kernel):
    a = await _mk(seeded_kernel, "rg_upd")
    before = dict(seeded_kernel.ctx.get(a) or {})
    for key, value in (
        ("handler_module", "file_bridge.tools"),
        ("root_path", "/tmp/elsewhere"),
        ("ctx", "x"),
        ("parent", "y"),
    ):
        r = await _kernel_call(seeded_kernel, type="update_agent", id=a, **{key: value})
        assert r.get("reason") == "bad_arg", (key, r)
        assert r.get("arg") == key, (key, r)
    after = dict(seeded_kernel.ctx.get(a) or {})
    assert after == before, "a refused update must change nothing"


async def test_create_agent_refuses_reserved_keys(seeded_kernel):
    r = await _kernel_call(
        seeded_kernel,
        type="create_agent",
        parent_id=seeded_kernel.id,
        handler_module="file_bridge.tools",
        allow=["."],
        id="rg_reserved",
        root_path="/tmp/elsewhere",
    )
    assert r.get("reason") == "bad_arg" and r.get("arg") == "root_path", r
    assert seeded_kernel.ctx.agents.get("rg_reserved") is None


# twin: an_unanswerable_verb_at_the_root_carries_reason_and_verb
async def test_a_verb_a_bare_agent_cannot_answer_is_coded(seeded_kernel):
    """The ROOT is a bare agent for anything outside its own verb set."""
    r = await _kernel_call(seeded_kernel, type="no_such_verb_at_all")
    assert r.get("reason") == "unhandled", r
    assert r.get("verb") == "no_such_verb_at_all", r


async def test_a_refusal_is_not_decorated_into_a_reflect(seeded_kernel):
    """A record may name a bundle THIS runtime cannot import (the browser's own
    agents persist here). The call is refused with a code — and is not dressed up
    with kernel_verbs/tree/more, which made it read like a healthy reflect."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "id": "rg_foreign",
                        "handler_module": "frontend_view.tools",
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == "rg_foreign", rec  # the record is legitimate
    r = (
        await seeded_kernel.send(
            [{"target": "rg_foreign", "payload": {"type": "reflect"}}]
        )
    )[0]["result"]
    assert r.get("reason") == "unhandled", r
    assert r.get("verb") == "reflect", r
    for decoration in ("kernel_verbs", "tree", "more"):
        assert decoration not in r, (decoration, r)


async def test_get_is_unhandled_not_a_kernel_verb(seeded_kernel):
    """`get` is NOT one of the kernel verbs — in any runtime, so a model never
    learns from one runtime's reflect a verb another does not have. A record is
    read with `list_agents` or a `reflect`.
    """
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "get", "id": seeded_kernel.id}}]
        )
    )[0]["result"]
    assert r.get("reason") == "unhandled", r
    assert r.get("verb") == "get", r
    assert "id" not in r, r

    kv = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "docs", "of": "kernel"}}]
        )
    )[0]["result"]["verbs"]
    assert "get" not in kv, kv
