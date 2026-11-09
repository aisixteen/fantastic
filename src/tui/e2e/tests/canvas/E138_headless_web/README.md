# E138 · headless_web — a recorded `web` host listens under `--headless`

Scripted, no model, no browser (`costs: free`). #560: `web` is the core listener, booted in every mode.

One `canvas` run records `web` and the canvas agents; then `fantastic --headless` runs in the same folder.

**PASS** when every check holds:

| check | what |
|---|---|
| a1 | headless serves the recorded `web` on its port: the canvas page answers 200 with HTML |
| a2 | a declared route works: a host call through the `canvas_ws` socket answers (`reflect` of the root answers with its id, no error) |
| b1 | a second agent's route on the same host: `static_files` serves a file that was allowed on it |
| b2 | the process is headless: no `the canvas:` announcement after the headless start (the record alone served) |
