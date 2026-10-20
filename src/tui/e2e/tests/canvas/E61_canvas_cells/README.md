# E61 · canvas_cells — nothing floats; cells, peers, coding agents, page bundles

Scripted, no model (`costs: free`). A FAKE `claude` first on PATH stands in for a
real coding agent (it prints a marker, then echoes what is typed).

**PASS** when every check holds:

| check | what |
|---|---|
| bare | a fresh canvas has no top bar, no sidebar, no cell |
| coding_listed | the double-click's `host · coding agents` lists the fake, its full path as the hint (`user path`) |
| coding_clicked · coding_runs · coding_echoes | its tile starts it in a terminal cell; it prints its marker and echoes a typed line |
| coding_cell · coding_exact | the cell's `command` is the full path; its exec agent allows exactly that line |
| tree_cell · tree_lists_backend | a `file tree` cell from the double-click; the terminal's host agent is a folder in it |
| canvases_cell | a `canvases` cell marks the open canvas |
| peer_opened · peer_empty · peer_marks_elsewhere · peer_own_records | `+ new canvas` opens `canvas_second`: empty, its file tree shows the whole project and marks the first canvas's agent `.elsewhere`, with no ↗ to open it; its own records and guid |
| bundle_runs · bundle_reloads | a page bundle (`base: html_agent`, `html: \|`) in the peer's own `bundles/` runs its seeded instance; an edit of its bundle.yaml re-renders it |
