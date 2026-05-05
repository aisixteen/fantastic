"""canvas_webapp — spatial UI agent."""

from __future__ import annotations


async def _make(kernel, upstream_id="some_backend"):
    rec = await kernel.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "canvas_webapp.tools",
            "upstream_id": upstream_id,
        },
    )
    return rec["id"]


async def test_reflect_returns_upstream_id(seeded_kernel):
    aid = await _make(seeded_kernel, upstream_id="canvas_backend_x")
    r = await seeded_kernel.send(aid, {"type": "reflect"})
    assert r["upstream_id"] == "canvas_backend_x"
    assert "get_webapp" in r["verbs"]


async def test_get_webapp_returns_descriptor(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "get_webapp"})
    assert r["url"] == f"/{aid}/"
    assert r["default_width"] == 800
    assert r["default_height"] == 600
    assert r["title"] == "canvas"


async def test_unknown_verb_errors(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "garbage"})
    assert "error" in r


async def test_render_html_uses_list_members_not_list_agents(seeded_kernel):
    """Drift guard: the served HTML must read membership from the
    upstream canvas_backend's list_members verb, NOT auto-discover via
    core.list_agents. Two canvases would otherwise auto-include the
    same set of webapps."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    html = r["html"]
    assert "list_members" in html, "refresh() must call upstream.list_members"
    assert "members_updated" in html, (
        "must subscribe to upstream's members_updated event"
    )
    assert "add_agent" in html, "dblclick must auto-add the new pair to this canvas"
    # Auto-discover removed: list_agents stays for record lookup but not
    # as the membership source.
    assert "myBundle" not in html, (
        "same-bundle exclusion was an auto-discover artefact; should be gone"
    )


async def test_render_html_streams_lifecycle_does_not_poll(seeded_kernel):
    """Canvas must be purely event-driven: one refresh on mount, then
    react to the streamed `agent_updated` / `agent_deleted` /
    `members_updated` events. A `setInterval(refresh, …)` would mean
    every canvas tab continuously hammers the kernel, mints noisy
    self-traffic in the agent-vis, and masks dropped fanouts.
    """
    aid = await _make(seeded_kernel)
    html = (await seeded_kernel.send(aid, {"type": "render_html"}))["html"]
    assert "setInterval" not in html, (
        "canvas must stream lifecycle events, not poll on a timer"
    )
    # The streaming wiring must still be in place — these are the
    # only path a no-poll canvas has to stay consistent.
    for tok in ("t.watch('core')", "agent_updated", "agent_deleted", "members_updated"):
        assert tok in html, f"missing stream wiring: {tok}"


async def test_render_html_uses_liquid_glass_chrome(seeded_kernel):
    """The canvas chrome must remain Liquid Glass — flatten back to
    solid fills and the candy is gone. Tokens to keep alive:
      - backdrop-filter on .agent-frame
      - the ::before specular layer
      - the inset top highlight in the box-shadow stack
      - the inline SVG refraction filter
    """
    aid = await _make(seeded_kernel)
    html = (await seeded_kernel.send(aid, {"type": "render_html"}))["html"]
    assert "backdrop-filter" in html, "lost the frosted-glass blur"
    assert ".agent-frame::before" in html, "lost the specular highlight layer"
    assert "inset 0 1px 0 rgba(255" in html, "lost the inner top highlight"
    assert "liquid-distort" in html, "lost the SVG refraction filter"
    # Negative: the old hard-edge defaults must be gone.
    assert "background: #0e0e16" not in html
    assert "border: 1px solid #333" not in html


async def test_render_html_dispatches_on_two_verbs(seeded_kernel):
    """Drift guard: canvas's frame manager probes BOTH get_webapp and
    get_gl_view per member, and installs each independently. An agent
    answering both verbs gets both presentations."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    html = r["html"]
    assert "type: 'get_webapp'" in html, "must probe get_webapp"
    assert "type: 'get_gl_view'" in html, "must probe get_gl_view"
    assert "Promise.all" in html or "Promise.allSettled" in html, (
        "must probe both verbs concurrently"
    )


async def test_render_html_has_gl_host_scaffolding(seeded_kernel):
    """Drift guard: canvas hosts a generic GL view registry that
    compiles each agent's `source` and runs it with cleanup-aware
    closures. Per-frame ticks via onFrame."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    html = r["html"]
    assert "glViews" in html, "must keep a Map of installed GL views"
    assert "installGlView" in html
    assert "removeGlView" in html
    assert "glFrameCbs" in html, "per-frame tick registry for GL views"
    assert "new Function(" in html, (
        "GL view source is compiled via new Function in the host"
    )
    assert "'THREE'" in html and "'scene'" in html and "'cleanup'" in html, (
        "GL view contract injects (THREE, scene, t, onFrame, cleanup)"
    )


async def test_render_html_no_inline_bganim(seeded_kernel):
    """Negative drift guard: bganim is GONE — no particle pipeline,
    no per-particle render fn, no compile/hot-reload wiring. Particle
    effects come back later as a peer GL agent."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    html = r["html"]
    assert "PARTICLE_COUNT" not in html
    assert "loadAndCompile" not in html
    assert "renderFn" not in html
    assert "bganim_updated" not in html
    assert "default_bganim" not in html
    assert "set_bganim" not in html
    assert "get_bganim" not in html


async def test_render_html_no_inline_agent_vis(seeded_kernel):
    """Negative drift guard: the agent-vis (sprites + slots + blip)
    no longer lives in canvas_webapp HTML — it's been lifted into the
    telemetry_pane bundle's glview.js and reaches the canvas only
    when telemetry_pane is added as a peer."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    html = r["html"]
    for token in (
        "agentSprites",
        "ensureAgentSprite",
        "removeAgentSprite",
        "triggerBlip",
        "assignSlot",
        "slotFreeList",
        "subscribeState",
    ):
        assert token not in html, (
            f"'{token}' must NOT live in canvas_webapp HTML — it belongs "
            f"to telemetry_pane's glview.js"
        )
