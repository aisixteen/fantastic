# E131 import_refusals

`user.import_subtree` (#269) against a running `fantastic canvas`, on blueprint
folders written by hand under the project:

1. an agent carrying `api_key: sk-planted…` is refused `secret_found`; the message names `agent a: a api_key key` and says `export again`, and never holds the key;
2. a folder with a handler that is not a built-in (`py_ab12.tools`) and files `s/bundle.yaml`, `s/tools.py` is refused `code_not_allowed`, naming the handler, the file and `allow_code: true`;
3. a folder with a code file next to a built-in handler is refused the same way, and with `allow_code: true, approve: true` it imports (the gate is the refusal, not the folder);
4. `approve` or `allow_code` given as the string `"true"` is not a yes: the first only previews, the second is still refused;
5. a folder with no `blueprint.yaml` is `not_found`, naming the file.
6. a lead holding a line on `credentials` (bound through a port), which the importing `mcp` leg does not hold, is refused: its create names the line the leg lacks (`'mcp' holds no call 'get' on 'credentials'`: the leg's `user` line shows `credentials`, and a writer hands out only what it holds) and nothing is kept.
7. an import aimed outside `canvas_root` (under `files`) is refused: the import is checked as the `mcp` leg, whose create line is on `canvas_root` only, so the grant check refuses the create, naming what the leg lacks (`'mcp' holds no create of 'ram_store.tools' under 'files'`, #792), and nothing is kept.

Every refusal leaves the agent count as it was; for 7, which the leg cannot see, the saved agent records on disk (the project's `.fantastic` and the home) are compared too. Scripted (no model, no key); needs no browser.
