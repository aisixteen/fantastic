# `peer_ring` (E169) — three peers ask each other in a ring, with no deadlock

Free: no model. Three local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the
models of three peers `ring_a`, `ring_b`, `ring_c` — the kernel's own `ai_chat.tools`
chats, each completing through its own `vllm_backend` provider — on the PACKAGED host of
`RUNTIME`, driven over `web_rest`. The memory store is a `sqlite_store`, so the rows
outlive a kill.

The ring (#795, everything async): A asks B, B asks C, C asks A, all at once. Each
peer's first answer is a `send` to the next peer with its question, then `done`. A peer
that reads a question answers it with its own `send` back to the asker, `re` set to the
asker's `msg`, then `done`. An answer read is ended with `done`.

Checks: each kick is acked at once (`accepted`, before any pass); every peer gets the
answer to its question, from the peer it asked, with `re` naming its own question's
`msg`; no peer task is left in `ps`; the ring ends well inside the mock's own time (no
600 s ceiling, the E37 deadlock); each peer ran 2 or 3 passes (3 when its question's
answer came in a pass of its own), counted from its rows (assistant rows = requests).

The restart leg: B is asked once more and its model holds the request; the kernel
process group is killed while that request is out, and a new kernel boots on the same
folder. B's pass re-runs ONCE over the message it never answered (nothing acked is
lost), and nothing else of B runs.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id peer_ring --lanes 3
```
