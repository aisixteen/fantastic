# E66 render_fidelity

The shared-WebGL terminal renderer under a seeded glyph/colour sweep
(`glyphs.py`): K terminals (`FIDELITY_K`, default 12), `FIDELITY_ROUNDS`
(default 6). Every frame: no placeholder-red pixel in any terminal. Every round:
each terminal equals its fresh redraw after a SIBLING clears the shared atlas,
and after its own clear. Then a lost WebGL context: every terminal back as it
was. Reports atlas pages live / released, uploads, contexts made.

    cd src/lib/python && CANVAS_HEADFUL=1 uv run python ../../tui/e2e/tests/canvas/E66_render_fidelity/probe.py
