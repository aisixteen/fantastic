# `secret_marks` (E113) — secret marks on a send item, over the wire

Free: no model. A `file_bridge` under the REST leg on the packaged host of
`RUNTIME`; marked calls arrive through both wire faces (REST
`X-Fantastic-Secret`, a ws `call` frame's `secret`) and a nested mark rides an
`update_agent`. It proves the handler gets the CLEAR value (file on disk,
read-back, stored record) while the call rows in the memory store (both the
onward call and the leg's `inbound_call`, marked under `item.payload.`), the
state stream (`payload`, `summary`) and the inbox mirror to a watcher show
`[secret]`; a mark naming nothing is skipped; a malformed field or a `type` mark
is `bad_arg` `secret` and the refused write never runs. The checks are listed in
`probe.py`'s docstring.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id secret_marks       # all three
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E113_secret_marks/probe.py
```
