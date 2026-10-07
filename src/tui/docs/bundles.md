> Folder bundles: a BUNDLE is code in `.fantastic/bundles/<name>/`; an AGENT is an instance of it. Edit the bundle and every instance changes at once.

# Bundles and their agents

Two things, never mixed up:

| | a BUNDLE | an AGENT (instance) |
|---|---|---|
| what | CODE: `bundle.yaml` (its verbs), `agent.py`, `panel.html` (its view) | a RECORD that names the bundle (`handler_module: <bundle name>`) |
| where | `.fantastic/bundles/<name>/` | `.fantastic/agents/…/<id>/agent.yaml` |
| change it | write its files — EVERY instance restarts at once | `update_agent` its record (fields), or send it its verbs |
| one of | per project (a folder) | many per bundle |

## Make one

From the canvas: double-click → **python agent** (host group). That copies the
CLI's starter into a NEW bundle (`.fantastic/bundles/py_<hex>/`) and places one
instance (`py_<hex>_1`), its view on the canvas.

## Change it (instantly)

- **logic**: edit `agent.py`; a new verb is a function in `fa.serve({...})` AND a
  line in `bundle.yaml` `verbs` (they must match — the kernel refuses an agent whose
  hello differs). Saving restarts every instance ABRUPTLY (in-flight calls answer
  `reloaded`).
- **view**: edit `panel.html`; every instance's cell re-renders (no restart).
  `{{agent}}` in it is filled with the instance's id.
- Files are written through `files` (`write {path: ".fantastic/bundles/<name>/agent.py", bytes}`).

## Keep it, or bundle it

- **leave it**: the instance stays in the tree (dehydrates and hydrates); the bundle
  stays in its folder.
- **a release / a variant** — so the working one and the copy live their OWN lives:
  copy the folder to a NEW name (`cp -r .fantastic/bundles/py_ab12 .fantastic/bundles/web_search`
  through an exec agent, or files read+write). The new name is a new handler: its
  instances are untouched by edits to the original, and the reverse.
- **update a release from the working one**: copy the working folder's files OVER
  the release folder — every instance of the RELEASE restarts with the new code.
- Never edit a bundle to change ONE agent: every instance of it changes. Fork first.

## Share it

A bundle is a folder of open files: zip it, send it, drop it into another
project's `.fantastic/bundles/`; read it and adapt it before running it.

## Trust

A python agent's code runs with your OS rights: what it does itself (processes,
files, network) passes no kernel gate. Run code you have read.
