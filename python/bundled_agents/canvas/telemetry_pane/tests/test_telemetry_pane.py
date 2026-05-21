"""telemetry_pane bundle tests.

Drift guards on:
- The verb surface (reflect lists get_gl_view).
- The shape of get_gl_view's response (`source` + `title`).
- The lifted glview.js source carries the expected Three.js + state
  stream tokens, and the cleanup discipline is followed.
- Critical: NO kernel calls in the render path so a self-visualizing
  instance does not feedback-loop.
"""

from __future__ import annotations


async def _make(kernel):
    rec = await kernel.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "telemetry_pane.tools",
        },
    )
    return rec["id"]


async def test_reflect_lists_get_gl_view(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "reflect"})
    assert "get_gl_view" in r["verbs"]
    assert "reflect" in r["verbs"]


async def test_get_gl_view_returns_source_with_title(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "get_gl_view"})
    assert isinstance(r.get("source"), str)
    assert len(r["source"]) > 200
    assert r.get("title") == "telemetry"


async def test_glview_source_uses_three_sprite_and_canvas_texture(seeded_kernel):
    aid = await _make(seeded_kernel)
    src = (await seeded_kernel.send(aid, {"type": "get_gl_view"}))["source"]
    assert "THREE.Sprite" in src, "agent vis must render as Three.js Sprite"
    assert "THREE.CanvasTexture" in src, "must back sprites with a CanvasTexture"
    assert "agentGroup" in src and "agentSprites" in src
    assert "ensureAgentSprite" in src
    assert "removeAgentSprite" in src
    assert "triggerBlip" in src


async def test_glview_source_subscribes_to_state_stream(seeded_kernel):
    aid = await _make(seeded_kernel)
    src = (await seeded_kernel.send(aid, {"type": "get_gl_view"}))["source"]
    assert "t.subscribeState" in src, "must drive the vis from the kernel state stream"
    # All five lifecycle/traffic kinds dispatched.
    for kind in ("'added'", "'removed'", "'updated'", "'drain'"):
        assert kind in src, f"missing dispatch for kind {kind}"


async def test_glview_source_pushes_cleanup(seeded_kernel):
    """remove_agent must tear down sprites + textures + subscription.
    The contract: source pushes closures into `cleanup` array."""
    aid = await _make(seeded_kernel)
    src = (await seeded_kernel.send(aid, {"type": "get_gl_view"}))["source"]
    assert "cleanup.push(" in src
    assert "unsubState()" in src, "cleanup must unsubscribe from state stream"
    assert "texture.dispose()" in src, "cleanup must dispose textures"
    assert "scene.remove(agentGroup)" in src, "cleanup must drop the group"


async def test_glview_source_does_not_call_kernel(seeded_kernel):
    """CRITICAL drift guard against feedback loops.

    Visualizing yourself must not produce more telemetry events.
    The render path is a pure consumer of state events; calling
    kernel verbs from inside it would create a multiplier.
    """
    aid = await _make(seeded_kernel)
    src = (await seeded_kernel.send(aid, {"type": "get_gl_view"}))["source"]
    assert "t.call(" not in src, "render path must not kernel.send"
    assert "t.send(" not in src, "render path must not kernel.send"
    assert "t.emit(" not in src, "render path must not kernel.emit"


async def test_glview_source_has_pulse_and_rays(seeded_kernel):
    """Drift guard on the Tron-legacy pulse + connection-ray visuals.

    The render path drives a per-frame glow decay (not setTimeout) and
    draws fading lines from sender → recipient sprite when the kernel
    state event carries a real sender id.
    """
    aid = await _make(seeded_kernel)
    src = (await seeded_kernel.send(aid, {"type": "get_gl_view"}))["source"]
    # Per-frame decay loop (replaces old setTimeout blip).
    assert "onFrame(" in src, "pulse decay must run on the host's per-frame tick"
    assert "GLOW_DECAY" in src, "glow must decay (not snap on/off)"
    # Connection rays — built from oriented PlaneGeometry quads
    # (LineBasicMaterial.linewidth is silently ignored on most WebGL
    # drivers, so we can't get fat lines that way).
    assert "addRay(" in src
    assert "raysGroup" in src
    assert "PlaneGeometry" in src, "fat rays use PlaneGeometry quads, not THREE.Line"
    assert "RAY_LAYERS" in src, "rays must have layered halo+core for the glow"
    # Traveling pulse along the wire.
    assert "addPulse(" in src, "rays must spawn a traveling pulse from sender"
    assert "PULSE_DURATION" in src
    # Sender-driven ray creation.
    assert "evt.sender" in src, "rays must spawn from kernel state event sender"


async def test_glview_source_has_wobble_and_message_pane(seeded_kernel):
    """Drift guard on the floating water-wobble + last-N message pane."""
    aid = await _make(seeded_kernel)
    src = (await seeded_kernel.send(aid, {"type": "get_gl_view"}))["source"]
    # Wobble.
    assert "WOBBLE_AMP_X" in src and "WOBBLE_AMP_Y" in src
    assert "baseX" in src and "baseY" in src, "wobble must offset from a stored base"
    # Per-sprite random phase prevents grid-wide sync.
    assert "Math.random()" in src
    # Messages pane.
    assert "messageLog" in src
    assert "MESSAGE_LOG_MAX" in src
    assert "redrawMessagePane" in src
    assert "msgSprite" in src
    # Pane consumes the kernel-side trimmed payload summary.
    assert "evt.summary" in src, "pane must read the state-event summary"


async def test_unknown_verb_errors(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "garbage"})
    assert "error" in r
