"""canvas_webapp — spatial UI agent."""

from __future__ import annotations


async def _make(kernel):
    rec = await kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "canvas_webapp.tools"},
    )
    return rec["id"]


async def test_reflect_returns_upstream_id(seeded_kernel):
    """First boot creates canvas_backend as a child; reflect surfaces
    its id via upstream_id."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "reflect"})
    assert r["upstream_id"] is not None
    assert r["upstream_id"].startswith("canvas_backend_")
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
    """The canvas chrome is Liquid Glass. Tokens to keep alive:
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


async def test_render_html_dblclick_adds_terminal_by_handler_module(seeded_kernel):
    """Drift guard: double-click on empty canvas must spawn a terminal
    via `canvas_backend.add_agent` with `handler_module:
    'terminal_webapp.tools'` — the canvas spawns + boots it as its own
    child (terminal_webapp._boot then spawns the terminal_backend). A
    prior version pre-created the pair on `core` and called add_agent
    with `agent_id`, which the current add_agent (spawn-from-
    handler_module) rejects — leaking two orphan agents and never
    joining the canvas."""
    aid = await _make(seeded_kernel)
    html = (await seeded_kernel.send(aid, {"type": "render_html"}))["html"]
    assert "dblclick" in html
    assert "handler_module: 'terminal_webapp.tools'" in html, (
        "dblclick must add_agent by handler_module, not adopt by agent_id"
    )
    assert "terminal_backend.tools" not in html, (
        "dblclick must let the canvas spawn the subtree, not pre-create "
        "a terminal_backend on core (orphan leak)"
    )


async def test_render_html_zoom_is_smoothed(seeded_kernel):
    """Drift guard: wheel zoom is lerped, not stepped. The wheel must
    only nudge `targetZ`; the rAF loop glides `view.z` toward it. If
    the wheel handler goes back to setting `view.z` directly the
    smoothing is silently gone."""
    aid = await _make(seeded_kernel)
    html = (await seeded_kernel.send(aid, {"type": "render_html"}))["html"]
    assert "targetZ" in html, "lost the smoothed-zoom target"
    assert "ZOOM_LERP" in html, "lost the per-frame lerp factor"
    # The wheel handler must drive targetZ, not view.z, or it's a jump.
    assert "targetZ = Math.max(ZOOM_MIN" in html, (
        "wheel must set targetZ (clamped), not mutate view.z directly"
    )


async def test_render_html_gl_views_are_containerized_and_live(seeded_kernel):
    """Each GL view runs in its own THREE.Group container (iframe
    analogue) and reloads in place: gl_agent.set_gl_source emits
    `gl_source_changed` → the canvas calls updateGlView → dispose the
    group + recompile, scoped to one view. No remove/re-add, no
    canvas refresh."""
    aid = await _make(seeded_kernel)
    html = (await seeded_kernel.send(aid, {"type": "render_html"}))["html"]
    # Per-view container — the source's injected `scene` is a Group.
    assert "new THREE.Group()" in html, "GL views must get a per-view Group container"
    assert "disposeObject3D" in html, "must free GPU memory on teardown"
    # In-place reload path.
    assert "updateGlView" in html, "must have an in-place GL reload path"
    # The canvas watches each GL member so the emit reaches it.
    assert "t.watch(agent_id)" in html, "installGlView must watch the GL member"
    # The producer event is wired.
    assert "gl_source_changed" in html, (
        "must listen for gl_source_changed → updateGlView"
    )
