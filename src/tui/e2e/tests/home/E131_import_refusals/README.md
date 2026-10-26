# E131 import_refusals

`user.import_subtree` (#269) against a running `fantastic canvas`, on blueprint
folders written by hand under the project:

1. an agent carrying `api_key: sk-planted…` is refused `secret_found`; the message names `agent a: a api_key key` and says `export again`, and never holds the key;
2. a folder with a handler that is not a built-in (`py_ab12.tools`) and files `s/bundle.yaml`, `s/tools.py` is refused `code_not_allowed`, naming the handler, the file and `allow_code: true`;
3. a folder with a code file next to a built-in handler is refused the same way, and with `allow_code: true, approve: true` it imports (the gate is the refusal, not the folder);
4. `approve` or `allow_code` given as the string `"true"` is not a yes: the first only previews, the second is still refused;
5. a folder with no `blueprint.yaml` is `not_found`, naming the file.

Every refusal leaves the agent count as it was. Scripted (no model, no key); needs no browser.
