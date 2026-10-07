# E60 bundle_flow — the owner's python-agent workflow, a → f

A BUNDLE is code (`.fantastic/bundles/<name>/`); an AGENT is an instance naming it
(`fantastic readme bundles`). Checks, in order (each depends on the last):

| step | what | checks |
|---|---|---|
| a | an AI is on the canvas (the plug) | `ai_ready` |
| b | double-click → **python agent** | `bundle_made` (a new `bundles/py_*` folder with `agent.py`), `instance_made` (`py_*_1`), `view_shown` (a host-side cell with the starter panel), `hello_works` (its button answers through the kernel) |
| c | edit `agent.py`, then `panel.html` | `logic_live` (the next hello answers the NEW greeting — the bundle reloaded), `view_live` (the cell re-renders the new panel, no restart) |
| d | restart the host | `hydrated` (the instance comes back and answers) |
| e | copy the folder to `bundles/release_agent` and place an instance | `release_loaded`, `independent` (editing the working bundle leaves the release's answer alone) |
| f | ask the AI twice | `ai_updates_release` (the release answers the AI's new greeting, the working bundle untouched), `ai_overwrites_release` (the release now runs the working bundle's code) |
