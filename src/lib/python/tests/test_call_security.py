"""Call security wired into the kernel — the gate in `Agent.send`.

Deny-all is the law for AGENT-originated sends; OPERATOR sends (no
`_current_sender` — the CLI / bootstrap / loader-at-boot / a test driver calling
`kernel.send` directly) are the root of trust and pass ungated.

We drive an agent-originated send by entering `sender_context(<agent_id>)`
around `kernel.send`, exactly as a handler's nested send is attributed.
"""

from __future__ import annotations


from kernel import sender_context


async def _mk(kernel, **meta) -> str:
    """Create a plain file_bridge child of root (opened so its OWN ingress gate
    never masks the call gate we're testing), returning its id. Created as
    operator → carries the stamped Unix default grant."""
    payload = {
        "type": "create_agent",
        "handler_module": "file_bridge.tools",
        "ingress_rule": "allow_all",
    }
    payload.update(meta)
    rec = await kernel.send(kernel.id, payload)
    assert "id" in rec, rec
    return rec["id"]


# ─── operator vs agent boundary ─────────────────────────────────


async def test_operator_send_is_ungated(seeded_kernel):
    # No sender_context → operator → anything passes, including mutation.
    a = await _mk(seeded_kernel)
    r = await seeded_kernel.send(a, {"type": "list", "path": ""})
    assert "error" not in r or r.get("reason") != "unauthorized"


async def test_agent_send_to_ungranted_peer_is_denied(seeded_kernel):
    a = await _mk(seeded_kernel)
    b = await _mk(seeded_kernel)
    with sender_context(a):
        r = await seeded_kernel.send(b, {"type": "list", "path": ""})
    assert r.get("reason") == "unauthorized"
    assert r.get("hint")  # the denial teaches how to open it


async def test_agent_may_call_self_and_parent_by_default(seeded_kernel):
    a = await _mk(seeded_kernel)
    with sender_context(a):
        # $self
        r_self = await seeded_kernel.send(a, {"type": "list", "path": ""})
        # $parent is the root (kernel_state); list_agents is a non-privileged verb
        r_parent = await seeded_kernel.send(seeded_kernel.id, {"type": "list_agents"})
    assert r_self.get("reason") != "unauthorized"
    assert r_parent.get("reason") != "unauthorized"


async def test_agent_cannot_create_agent_even_on_its_parent(seeded_kernel):
    # A depth-1 agent's $parent IS the root, and its default grant is "*" — but
    # create_agent is PRIVILEGED, so the wildcard does not cover it.
    a = await _mk(seeded_kernel)
    with sender_context(a):
        r = await seeded_kernel.send(
            seeded_kernel.id,
            {"type": "create_agent", "handler_module": "file_bridge.tools"},
        )
    assert r.get("reason") == "unauthorized"


async def test_reflect_is_always_ungated(seeded_kernel):
    a = await _mk(seeded_kernel)
    b = await _mk(seeded_kernel)
    with sender_context(a):
        r = await seeded_kernel.send(b, {"type": "reflect"})
    assert r.get("reason") != "unauthorized" and r.get("id") == b


async def test_explicit_lateral_grant_opens_exactly_it(seeded_kernel):
    b = await _mk(seeded_kernel)
    # a is granted `list` on b, nothing else.
    a = await _mk(seeded_kernel, can_call={"$self": "*", "$parent": "*", b: ["list"]})
    with sender_context(a):
        ok = await seeded_kernel.send(b, {"type": "list", "path": ""})
        no = await seeded_kernel.send(b, {"type": "mkdir", "path": "x"})
    assert ok.get("reason") != "unauthorized"
    assert no.get("reason") == "unauthorized"


async def test_explicit_kernel_grant_lets_an_agent_create(seeded_kernel):
    # Grant a the privileged verb explicitly → it can mutate through the root.
    a = await _mk(
        seeded_kernel,
        can_call={"$self": "*", seeded_kernel.id: ["create_agent"]},
    )
    with sender_context(a):
        r = await seeded_kernel.send(
            seeded_kernel.id,
            {"type": "create_agent", "handler_module": "file_bridge.tools"},
        )
    assert r.get("reason") != "unauthorized" and "id" in r
