# E73 `cell_chrome` — a cell's border, corners and hover cost

Owner 2026-10-11: a cell's chrome must show on its REAL border — with the pointer a little above or beside the cell, not only over its content — and it resizes from every corner. The hover must cost nothing measurable.

1. pointer away → chrome hidden; 10px above the cell / 10px right of it → its head shows;
2. each corner handle (nw, ne, sw, se) dragged → the size changes and the OPPOSITE corner stays put;
3. six cells over an animated background: frame rate and the GPU process's CPU with the pointer at rest vs sweeping over the cells.
