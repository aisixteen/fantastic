# E90 `tree_drag` — drag from the file tree onto the canvas

#134 and #135: the tree's rows drag onto the canvas, and a drop is the view's
`open` verb. Seeded: a `file tree` cell (`tree_e90`), two terminal cells
(`cell_e90a`, `cell_e90b`) each backed by a host exec agent that ticks, and the
project's `src/one.txt` ("one") and `docs/a.md`. The drop is dispatched in the
page with a `DataTransfer` (a CDP drag is not reliable), on the tree's rows and
on `#canvas`.

1. the tree lists both agents; both cells draw their ticks;
2. `view_root hide` on both: their frames go, their rows are `.hidden`;
3. a click on one row and a ⌘/Ctrl-click on the other mark both (`.marked`);
4. the marked rows drag onto the canvas: both cells come back, their ticks
   advance (live), and the frame count is what it was (no duplicate);
5. `src/one.txt` dragged in: a preview pinned to it shows "one", and "two" written
   on disk shows in it;
6. `docs` dragged in: a file tree rooted at it lists `docs/a.md`, not `src`;
7. the cells this probe made are removed; no page error is logged;
8. the canvas's own sidebar (#148, the file tree fixed beside the canvas): zoomed
   far out (`wheel`), it stays at the viewport's left edge; its `reveal` runs
   through the root view's `tree` verb; both cells hidden again; the agent row
   dragged from the sidebar brings its cell back centred on the drop point; the
   file row dragged from it opens a preview centred on its drop point.
