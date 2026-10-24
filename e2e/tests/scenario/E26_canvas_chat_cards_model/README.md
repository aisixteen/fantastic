# E26 · inline agent views in a chat, with a real model (#339)

**Costs:** turns. One model, asked for a card in the chat: it creates a host agent with a
`view` and `embed`s it. The run checks that the card fills the chat column's width, has
a defined height of at most 1200 px (`CHAT_CARD_MAX_HEIGHT`), and does not overflow.

Needs a plug (`BACKEND`, see `e2e/backends/`). With none named, or one whose requirements
are unmet, the run prints `E2E-SKIP:` and is reported as skipped.

    cd src/lib/python && BACKEND=vllm VLLM_ENDPOINT=… FANTASTIC_AI_MODEL=… \
      uv run python -u ../../../e2e/runners/canvas_chat.py canvas_chat_cards_model
