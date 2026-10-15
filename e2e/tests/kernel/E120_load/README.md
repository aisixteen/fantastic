# `kernel_load` (E120) — hydration and batch throughput at scale

Free: no model. Runs SOLO (`solo: true`): it measures time. On the PACKAGED host
of `RUNTIME`, over `web_rest`: the driver's seed boots alone (baseline), then with
1000 `ram_store` records on disk (hydrated at boot — every one listed by
`list_agents`, indexed by `reflect tree=ids`, a sample answering `reflect`); 500
REST `reflect`s over 16 client threads; and one AI turn (`vllm_backend` against a
scripted local server, `e2e/mocks/vllm.py`) answering with 500 `<call>`s — ONE
kernel batch to 500 hydrated agents, whose 500 replies come back as one turn.

Every number is printed (`NUMBERS <runtime>: {...}`); the bounds in `probe.py`
(`BOUNDS`) are generous and only catch a gross regression.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id kernel_load --lanes 1            # all three, serially
RUNTIME=swift uv run python ../../../e2e/tests/kernel/E120_load/probe.py
```
