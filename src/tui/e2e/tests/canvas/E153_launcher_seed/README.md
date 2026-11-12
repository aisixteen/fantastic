# E153 · launcher_seed — the launcher seeds a fresh state folder, then the kernel starts

Scripted, no model, no browser (`costs: free`). #555: the launcher (`main.rs`, or an embedder's own) seeds; the kernel and `main_with` never do.

**PASS** when every check holds:

| check | what |
|---|---|
| a | `--headless`, `canvas` and `mcp`, each in a fresh folder: the stock tree is written and the kernel holds the lock |
| b | no argument with no terminal, `send`, `import`, a refused `mcp` line and an unknown command write no `.fantastic` (and exit as documented: 0, 1, any, 2, 2) |
| c1 | the test-only embedder (`examples/embedder.rs`: the launcher plus ONE YAML blueprint, a `ram_store` named `embedder_store`; public API only) with `--headless --statedir <far> --workdir <work>`, run from a third folder: the stock tree and `embedder_store` are in the state folder and the kernel runs on it |
| c2 | the live `list_agents` (`send --statedir`) names the stock agents and `embedder_store` |
| c3 | no `.fantastic` in the cwd or the work folder |

It fails when the launcher's blueprint pass is skipped (c1, c2) or the seed goes to the cwd (c1, c3).
