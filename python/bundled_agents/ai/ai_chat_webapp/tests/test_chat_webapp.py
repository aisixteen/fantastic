"""ai_chat_webapp — provider-agnostic chat UI agent."""

from __future__ import annotations


async def _make(kernel, provider="ollama"):
    rec = await kernel.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "ai_chat_webapp.tools",
            "provider": provider,
        },
    )
    return rec["id"]


async def test_reflect_returns_upstream_id(seeded_kernel):
    """First boot creates the provider backend as a child; reflect
    surfaces its id via upstream_id."""
    aid = await _make(seeded_kernel, provider="ollama")
    r = await seeded_kernel.send(aid, {"type": "reflect"})
    assert r["upstream_id"] is not None
    assert r["upstream_id"].startswith("ollama_backend_")
    assert "get_webapp" in r["verbs"]


async def test_get_webapp_returns_descriptor(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "get_webapp"})
    assert r["url"] == f"/{aid}/"
    assert r["default_width"] == 360
    assert r["default_height"] == 480
    assert r["title"] == "chat"


async def test_unknown_verb_errors(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "garbage"})
    assert "error" in r


async def test_render_html_has_stop_button_and_interrupt_wiring(seeded_kernel):
    """Drift guard: served HTML must carry the send/stop button toggle
    that calls the upstream's `interrupt` verb to cancel mid-stream
    (Claude-Code-style cancel). Plus the queued/client_id wiring."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    html = r["html"]
    assert "interrupt" in html, (
        "must call upstream's interrupt verb to cancel mid-stream"
    )
    assert "type: 'interrupt'" in html or "'interrupt'" in html, (
        "the interrupt call must reach upstream"
    )
    # Per-client wiring (client_id from localStorage; queued event handling).
    assert "clientId" in html
    assert "ai_chat_client_id:" in html
    assert "queued" in html, "must mark message as queued when lock contended"


async def test_render_html_has_esc_to_interrupt(seeded_kernel):
    """ESC key halts the in-flight generation regardless of focus.
    Same semantic as clicking the stop button — calls the upstream's
    `interrupt` verb. Drift guard so a refactor can't silently drop it."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    html = r["html"]
    # A keydown listener must check for the Escape key.
    assert "'Escape'" in html, "must bind ESC key to interrupt"
    # Window-level binding (not just the input) so ESC works regardless
    # of focus.
    assert "window.addEventListener" in html or "document.addEventListener" in html, (
        "ESC must be bound at window/document level, not just on input"
    )
    # Status footer hint should mention esc so users know the binding exists.
    assert "esc" in html.lower(), "status footer hint must mention esc"


async def test_render_html_clears_stale_state_on_disconnect(seeded_kernel):
    """When the WS dies (server restart, sleep/wake), the UI must drop
    its in-flight + queued bubbles and surface a 'disconnected' hint —
    not stay stuck pretending to stream forever."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    html = r["html"]
    # Hook onto transport.js's lifecycle.
    assert "onLifecycle" in html, "must subscribe to transport lifecycle"
    assert "'disconnected'" in html, "must handle the 'disconnected' transition"
    # On disconnect, drop inflight + queue.
    assert "queuedBubbles.clear" in html, (
        "queue must be cleared on disconnect (no stale ⌛ bubbles)"
    )


async def test_render_html_has_status_pipeline_and_fifo(seeded_kernel):
    """The served HTML carries the status-stream pipeline (status verb
    on boot, status event subscription, phase pill, tool blocks, FIFO
    state)."""
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    html = r["html"]
    # Status verb call on boot.
    assert "type: 'status'" in html, "boot must call upstream's status verb"
    # Status event subscription.
    assert "t.on('status'" in html, "must subscribe to status events"
    # Tool block markup.
    assert 'class="tool-block' in html or "tool-block" in html
    assert "data-call-id" in html or "callId" in html
    # Status footer with phase pill.
    assert 'id="status-footer"' in html
    assert "phase-pill" in html
    # FIFO state.
    assert "queuedBubbles" in html, "must keep a Map of queued bubbles"
    # Boot snapshot is consumed.
    assert "mine_pending" in html
    assert "others_pending" in html
    # CSS pulse animation present.
    assert "@keyframes" in html or "keyframes pulse" in html
