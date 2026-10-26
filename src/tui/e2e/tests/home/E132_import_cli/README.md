# E132 import_cli

The real `fantastic import` command (#269) against a running `fantastic canvas`:

1. in a folder where no kernel runs it exits 1 saying so, and starts nothing;
2. without `--parent` it exits 2 and prints the usage;
3. off a terminal and without `--approve` it prints the preview (`would create 3 agents:`, `reaches`, `ports:`, `no code`), exits 0 and creates nothing;
4. on a pseudo-terminal it asks `approve? [y/N]`: `n` creates nothing, `y` imports and prints `imported 3:` with the new ids (skipped on Windows: no pty);
5. `--bind @memory=memory2 --approve` imports; a second run gives the next ids;
6. a refusal prints `refused (<reason>): …` and exits 1 (`unbound_ports` naming `pass bind`; `code_not_allowed` until `--allow-code`).

Scripted (no model, no key); needs no browser.
