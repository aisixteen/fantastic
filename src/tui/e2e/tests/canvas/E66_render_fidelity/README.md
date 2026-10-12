# E66 render_fidelity

The shared-WebGL terminal renderer under a seeded glyph/colour sweep
(`glyphs.py`): K terminals (`FIDELITY_K`, default 12), `FIDELITY_ROUNDS`
(default 6). Every frame: no placeholder-red pixel in any terminal. Every round:
each terminal equals its fresh redraw after a SIBLING clears the shared atlas,
and after its own clear. Then a lost WebGL context: every terminal back as it
was. Reports atlas pages live / released, uploads, contexts made.

A redraw "equals" when at most 64 pixels change (`TOL`): a glyph overhanging its
cell can land a texel column apart; a stale glyph is a whole cell wrong. The
sweep never uses red colours nor the emoji measured near pure red (`glyphs.py`
`RED_EMOJI`), so ANY red pixel is the placeholder.

    cd src/lib/python && CANVAS_HEADFUL=1 uv run python ../../tui/e2e/tests/canvas/E66_render_fidelity/probe.py
