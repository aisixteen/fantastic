# E25 · inline agent views in a chat (#339)

**Costs:** free. A scripted model, a real headless Chrome over CDP.

The chat cell of a canvas draws a host agent's panel as a CARD, where the AI's
`embed {id}` row sits in the conversation. The scripted model (`e2e/mocks/vllm.py`)
creates a host agent whose record carries a `view` (`kind: html`, `calls`) and a
`callable_by: {user: [...]}`, then `embed`s it by id.

| run | proves |
|---|---|
| `embed` | the card is in the chat cell, 100% of the column's width (within 2 px), its height defined and at most 1200 px; the embed reply the model reads carries the box `{width: 100%, max_height: 1200}` |
| `live-update` | the agent receives an update and the card redraws, with no polling |
| `overflow-hint` | a panel taller than the cap: the card is capped at 1200 px and scrolls inside, and the AI is told to redo the panel |
| `reload` | the page reloads: the cards come back live, in the same places |
| `restart` | the host restarts: the same |
| `delete-open` | the agent is deleted while the chat is open: its card turns "deleted" at once |
| `delete-closed` | deleted while the chat was closed: `[deleted from canvas]` after a reload; the press asks (gone) or re-attaches (back under the same id) |
| `unembed` | the card's menu removes it (`unembed {id}` on the AI); it stays gone after a reload |
| `canvas-and-chat` | one agent as a cell on the canvas and a card in the chat: hiding the cell leaves the card |

Runner: `e2e/runners/canvas_chat.py` (`canvas.py`'s host and page, plus the mock).

    cd src/lib/python && uv run python -u ../../../e2e/runners/canvas_chat.py canvas_chat_cards
