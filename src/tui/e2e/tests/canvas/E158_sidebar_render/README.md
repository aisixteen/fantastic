# E158 `sidebar_render` — the sidebar view as drawn in the canvas's tree cell (#608, #698, #714)

A fresh canvas places a cell for the host agent `sidebar`'s html view at its left, a cell like
any other (a null-origin
iframe: the probe reads it through CDP, as the chat probes read a chat). Seeded: the project's
`notes.txt` and `docs/a.md`. No other cell; part 8 adds two chats on a scripted model.

1. the tree cell is drawn at the canvas's left (not full screen) and the view's parts are drawn, in the mock's layout: the Kernels strip with this
   kernel highlighted, the header, the Agents nav with the tree, the
   filter field and the Files switch (off); the "Waiting for you" box is hidden (nothing asks);
2. agent rows are drawn: the root (depth 0), `user` and `files`; no file row yet;
3. the Files switch on: the project's `docs`, `notes.txt` (and the other root entries) are rows
   INSIDE the tree, one level below the `files` agent; opening `docs` shows `docs/a.md`;
4. a click on the `user` agent row opens its view on the canvas (a cell is drawn for it, not full screen);
5. a reload: the switch is still on and `docs/a.md` is drawn again without a click (the open
   state is kept by the page and given back to the view);
6. the filter narrows the tree to the match and the rows above it;
7. no page error is logged;
8. agent states (#714): two `vllm_backend` chats `c714a`, `c714b` on the parity OpenAI chat stub
   (a scripted model, no GPU; the newest message's marker picks the answer), made as the canvas
   makes every AI (`canvas_root create`). A has `transport_retries: 1` and `misfire_limit: 2`
   (the least), so its second failed or dead call in a row gives the chain up. Each state comes
   from the chat's real path and is checked on A's row (badge class, glyph, colours against the
   skin tokens, the word under the name, the face's filter):
   - at rest: no badge, no word;
   - Working: `thinking` (the stub holds its answer), `queued` (a second message meanwhile),
     `writing` (the answer's first line, then a hold), `retrying` (a stream that dies);
   - Done: a call that completed: ✓ on ok, kept after the call;
   - Failed: the retried stream dies again, the failed call handed back dies too, the chain
     gives up: × on error, kept;
   - Stalled: an empty answer (a dead call), twice: `!` on warn is drawn between the dead call
     and the next one (the chain rerolls at once, then gives up), so it is read from the row's
     drawn states (a MutationObserver), not from a still row;
   - Working, waiting: A hands B a slow message and calls `await`; the chain runs its calls
     first (B thinking, A's own call already done), then waits on the reply: `waiting` is drawn
     between B's answer and the turn it wakes, read from the drawn states;
   - Asks you: A's inquiry to the person: `?` on warn; answered, the badge goes;
   - Paused: the `pause` verb: II on muted, the face greyed; `resume` with nothing held: at rest.

   With `E158_SHOTS=<dir>` the probe saves the tree cell (`tree-<state>.png`) and A's and B's rows
   at 3x (`rows-<state>.png`; some also in the light view skin, `-light`) per state, and the
   mock's "Agent state" section light and dark (`mock-agent-state-<theme>.png`; the mock path is
   `TEAM_MOCK`, default the team checkout's `design/mock/states.html`).
