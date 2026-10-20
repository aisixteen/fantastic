# E89 `file_tree` — a live file tree, a previewer and an editor

#106: the project's folder as a tree (every agent a folder in it), a previewer
that follows its selection or is pinned to a path, and an editor with a loud
conflict. Seeded: a `file tree` cell (`tree_e89`), a preview pinned to
`src/one.txt` (`prev_pin`) and a preview that follows the tree (`prev_follow`);
`src/one.txt` ("one") and `docs/a.md` on disk. Every page call below is
`host_call` to `<page guid>|<cell>`, the way an AI reaches a cell.

1. the tree lists `src`, `docs` and `.fantastic` (highlighted);
2. a click on `src`'s triangle lists it;
3. `src/new.txt` written on disk appears in the open folder, no reload;
4. `src/one.txt` rewritten ("two") shows in the pinned preview's `.pv-code`;
5. `src/new.txt` removed: its row goes;
6. `select {path: docs/a.md}` replies `selected`, the row is `.sel`, and the
   follower renders an `h1`;
7. `select {agent: files}` finds the agent's folder (by its id, not its folder's
   name); the folder row has a badge; the follower shows its `agent.yaml`
   (containing `file_bridge`); the ⇄ chip shows the live agent (the host agent's
   own view: the reflect card naming `files`);
8. `edit` on the pinned preview opens the editor; " three" typed; ⌘S (Ctrl-S off
   macOS) → the disk reads "two three"; `select` of the file shows it in the
   follower;
9. " four" typed (unsaved), the disk written "disk wins": `.pv-conflict` shows,
   the disk still reads "disk wins", `save` replies `reason: conflict`, and
   Reload puts "disk wins" in the editor and clears the banner;
   then " five" typed and the disk written "disk two": Compare shows `.pv-diff`
   and writes nothing; Keep mine writes "disk wins five" to the disk;
   the filter field ("one.txt") narrows the loaded tree and hides `.fantastic`;
   ↓ from it focuses the tree, an arrow key sets the cursor row; clearing it
   restores the tree;
   collapsing `src` drops `src` from the bridge's holds (the same sentinel poll);
10. the three cells removed (`view_root remove`): the bridge's holds (the
    `paths` of `unwatch_path` on a sentinel, polled up to 10 s) name none of
    `.`, `src`, `docs`, `src/one.txt`, `docs/a.md`.
11. no page error is logged (WebGL noise aside).
