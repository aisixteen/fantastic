"""terminal_webapp — xterm UI agent.

In the recursive Agent model, terminal_webapp owns its terminal_backend
as a structural child. `_boot` (fired automatically on create_agent)
spawns the backend idempotently and writes its id into the webapp's
`upstream_id` field. Subsequent reboots find the existing child via
`_load_children` and skip the creation.
"""

from __future__ import annotations


async def _make(kernel):
    rec = await kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "terminal_webapp.tools"},
    )
    return rec["id"]


async def test_reflect_returns_upstream_id(seeded_kernel):
    """After create_agent → _boot fires → backend child spawned → its
    id stored on upstream_id. Reflect surfaces it for domain code
    (canvas frame chrome, etc.) that needs to know the pair."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "reflect"})
    assert r["upstream_id"] is not None
    assert r["upstream_id"].startswith("terminal_backend_")
    assert "get_webapp" in r["verbs"]
    # The backend exists in the substrate, parented under us.
    webapp_agent = seeded_kernel.ctx.agents[aid]
    assert r["upstream_id"] in webapp_agent._children


async def test_first_boot_spawns_backend_idempotent(seeded_kernel):
    """Boot is idempotent — second boot finds the existing child and
    does NOT create a duplicate."""
    aid = await _make(seeded_kernel)
    webapp_agent = seeded_kernel.ctx.agents[aid]
    children_before = list(webapp_agent._children.keys())
    assert len(children_before) == 1
    # Re-fire boot; should be a no-op.
    await seeded_kernel.send(aid, {"type": "boot"})
    children_after = list(webapp_agent._children.keys())
    assert children_after == children_before


async def test_cascade_kills_backend(seeded_kernel):
    """Deleting the webapp cascades through the backend (PTY would die
    if the backend's _shutdown ran for real; we just check structural
    removal here since spawning a real PTY in tests is heavy)."""
    aid = await _make(seeded_kernel)
    webapp_agent = seeded_kernel.ctx.agents[aid]
    backend_id = list(webapp_agent._children.keys())[0]
    assert backend_id in seeded_kernel.ctx.agents
    r = await seeded_kernel.send(seeded_kernel.id, {"type": "delete_agent", "id": aid})
    assert r["deleted"] is True
    assert aid not in seeded_kernel.ctx.agents
    assert backend_id not in seeded_kernel.ctx.agents


async def test_get_webapp_returns_descriptor(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "get_webapp"})
    assert r["url"] == f"/{aid}/"
    assert r["default_width"] == 600
    assert r["default_height"] == 400
    assert r["title"] == "xterm"


async def test_unknown_verb_errors(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "garbage"})
    assert "error" in r


async def test_render_html_has_flow_control_ack(seeded_kernel):
    """Drift guard: the served xterm UI must ack each output chunk
    AFTER xterm parses it (the term.write callback), buffered to one
    ack per CHAR_COUNT_ACK_SIZE chars. This is the consumer half of
    the VSCode-style backpressure — without it the backend's paused
    PTY reader never resumes and the terminal goes dead under a
    flood (e.g. a pasted script that runs)."""
    aid = await _make(seeded_kernel)
    html = (await seeded_kernel.send(aid, {"type": "render_html"}))["html"]
    assert "CHAR_COUNT_ACK_SIZE" in html, "lost the ack-buffer threshold"
    assert "type: 'ack'" in html, "must send the ack verb to the backend"
    # The ack must be wired through term.write's parse callback, not
    # fired blindly on receipt — that's what makes it true backpressure.
    assert "term.write(d, () => ackChars" in html, (
        "ack must fire from xterm's write/parse callback"
    )


async def test_render_html_has_image_paste_bridge(seeded_kernel):
    """Drift guard: the served xterm UI must bridge image paste.
    xterm only pastes text/plain, so a browser-clipboard image would
    be silently dropped — and the server-side `claude` can't reach
    the browser clipboard. The webapp catches the image item and
    ships the bytes to the backend's paste_image verb."""
    aid = await _make(seeded_kernel)
    html = (await seeded_kernel.send(aid, {"type": "render_html"}))["html"]
    assert "clipboardData" in html, "must inspect the paste event's clipboardData"
    assert "type: 'paste_image'" in html, "must call the backend paste_image verb"
    assert "it.type.startsWith('image/')" in html, (
        "must filter to image clipboard items"
    )
