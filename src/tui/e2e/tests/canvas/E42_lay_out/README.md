# E42 `lay_out`

Every ported project is a spatial arrangement of panels. Six slides scattered; asked to put them in one row, in order, left to right.

**PASS** (on disk, after the loader flush): slide 1..6 x strictly increasing, and all within one row height (dy ≤ 180). The AI moves cells with the canvas's `move` (or `update`) verb.
