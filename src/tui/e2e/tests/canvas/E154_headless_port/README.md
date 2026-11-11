# E154 · headless_port — `--port` and `--host` with `--headless`

Scripted, no model, no browser (`costs: free`). #580. One `canvas` run records `web` and the canvas page; then in the same folder:

**PASS** when every check holds:

| check | what |
|---|---|
| a1, a2 | `--headless --port N --host 127.0.0.1` (N a different port): the page is served on N and the `web` record names N |
| b1, b2 | `--headless` with no flags: served on N again, the record unchanged |
| c1 | the `web` record loses its `port`: `--headless` picks a free port, records it and serves on it |
| c2 | the next `--headless` serves on the same port, the record unchanged |
