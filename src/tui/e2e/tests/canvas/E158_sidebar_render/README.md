# E158 `sidebar_render` — the sidebar view as drawn in the canvas's left column (#608)

The canvas mounts the host agent `sidebar`'s html view in its left column (a null-origin
iframe: the probe reads it through CDP, as the chat probes read a chat). Seeded: the project's
`notes.txt` and `docs/a.md`. No cell, no model.

1. the column and the view's parts are drawn, in the mock's layout: the Kernels strip with this
   kernel highlighted, the header with "Hide the tree", the Agents nav with the title block, the
   filter field and the Files switch (off); the "Waiting for you" box is hidden (nothing asks);
2. agent rows are drawn: the root (depth 0), `user` and `files`; no file row yet;
3. the Files switch on: the project's `docs`, `notes.txt` (and the other root entries) are rows
   INSIDE the tree, one level below the `files` agent; opening `docs` shows `docs/a.md`;
4. a click on the `user` agent row opens its view on the canvas (a cell is drawn for it);
5. a reload: the switch is still on and `docs/a.md` is drawn again without a click (the open
   state is kept by the page and given back to the view);
6. the filter narrows the tree to the match and the rows above it;
7. "Hide the tree" collapses the column; no page error is logged.
