# `kernel_load` (E120) — hydration and batch throughput at scale

Free: no model. On the PACKAGED rust host, over `web_rest`: the driver's seed
boots alone (baseline; `rest`'s root reflect carries no runtime frame, the
operator's one-shot `kernel reflect` after the stop names it), then with 1000
`ram_store` records on disk (hydrated at boot — every one listed by `rest`'s paged
`reflect tree=ids`, indexed by the root's, a sample answering `reflect`); 500
REST `reflect`s over 16 client threads; and one AI turn (`vllm_backend` against a
scripted local server, `e2e/mocks/vllm.py`), the AI holding one `reflect` line per
target, answering with 500 `<call>`s — ONE
kernel batch to 500 hydrated agents, whose 500 replies come back as one turn.

Every number is printed (`NUMBERS <runtime>: {...}`); the bounds in `probe.py`
(`BOUNDS`) are generous and only catch a gross regression.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id kernel_load --lanes 1
```
