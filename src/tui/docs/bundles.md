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

An AI or a coding CLI writes a new bundle ONLY when the user asks for one;
otherwise it makes agents with `create_agent` from the bundles `reflect` lists.

## Change it (instantly)

- **logic**: edit `agent.py`; a new verb is a function in `fa.serve({...})` AND a
  line in `bundle.yaml` `verbs` (they must match — the kernel refuses an agent whose
  hello differs). Saving restarts every instance ABRUPTLY (in-flight calls answer
  `reloaded`).
- **view**: edit `panel.html`; every instance's cell re-renders (no restart).
  `{{agent}}` in it is filled with the instance's id. Editing the code and the
  panel together (one save, or a rewrite of the whole bundle) reloads the code
  AND re-renders the panel; the panel's `calls` in `bundle.yaml` reach every
  instance's grant.
- Files are written through `files` (`write {path: ".fantastic/bundles/<name>/agent.py", bytes}`).

## Keep it, or bundle it

- **leave it**: the instance stays in the tree (dehydrates and hydrates); the bundle
  stays in its folder.
- **a release / a variant** — so the working one and the copy live their OWN lives:
  copy the folder to a NEW name (`cp -r .fantastic/bundles/py_ab12 .fantastic/bundles/web_search`
  through an exec agent, or files read+write). The new name is a new handler: its
  instances are untouched by edits to the original, and the reverse.
- **place it again**: every bundle in `.fantastic/bundles/` is a tile in the
  double-click popup's **host · bundles** group — each click places one more
  instance (`<name>_<n>`). An agent does the same with `spawn {instance_of:
  <name>}` on this canvas's node (`canvas_root`).
- **an html cell, bundled**: write its body (`get_html` on `<guid>|<cell>`) into
  `.fantastic/web/bundles/<name>/bundle.yaml` — `base: html_agent`, `sentence:`,
  `html: |` and the body indented under it. It loads on the write (the page
  watches its `bundles/`), is a tile in the popup's **page · bundles** group, and
  each pick places one more instance (`<name>_<hex>`) whose body is the bundle's
  `html`; edit that `html` and every instance without its own `set_html` body
  draws it. The cell you bundled from keeps its own body.
- **update a release from the working one**: copy the working folder's files OVER
  the release folder — every instance of the RELEASE restarts with the new code.
- Never edit a bundle to change ONE agent: every instance of it changes. Fork first.

## Live panels

A host agent can PUSH updates into its own cell — a live panel, no polling. The
python agent starter (double-click → `python agent`; files in
`src/tui/starters/python_agent/`) is the minimal working one:

- `bundle.yaml` — the record carries the view:
  `view: {kind: html, bytes_file: panel.html, calls: [hello, count, watch]}`.
- `agent.py` — a verb does its work and EMITS: `fa.emit({"type": "count", "count": n})`.
- `panel.html` — calls with `fantastic.send(AGENT, {type: "count"})` and redraws
  on every event with `fantastic.watch(AGENT, ev => …)`.

Grow it the same way: verbs return data, an `emit` on each change, the panel
redraws. `innerHTML` runs no `<script>` — keep the panel's script in
`panel.html` and send data (or script-free HTML) in the events.

## Declared TUI views

The terminal UI draws an agent's own panel from DATA, with no Rust written: add
`tui` to the same `view` the web panel uses, in `bundle.yaml`. One file declares
both; `refresh_views` writes it to every instance like the rest of `view`, so an
edit shows at once (no restart), and a bundle with only `tui` (no `bytes_file`) is
fine.

`view.tui` is a list of 1 to 16 sections. Each section renders the reply of ONE
verb call:

| key | |
|---|---|
| `title` | the heading (a non-empty string) |
| `call` | `{verb, args?}`: the verb sent to the agent, `args` merged into the call |
| `show` | `kv` (label and value per field; no fields: every top-level scalar of the reply), `table` (rows from the reply if it is a list, else its first list-valued key; columns are the fields, or the first row's keys), `list` (one line per item), `log` (the same, newest last, scrolled to the newest line until you scroll up), `text` (the reply string, else the first field, else its `text`) |
| `fields` | optional: a dotted path (`load.1m`; a number indexes a list: `items.0.name`) or `{path, label?}`; the label defaults to the path |
| `every_s` | optional: refresh every N seconds (1 to 3600) while this agent is the selected one |

Any other key is refused, naming it. A refused declaration is refused with
`reason: bad_arg` and a sentence naming the section and the field
(for example: view.tui[1].show: `grid` is not a kind; the kinds are kv, table, list, log, text):
`spawn` and the bundle watcher report it, and the instances keep the view they had.

**Which panel an agent gets**: the built-in panels first (ai, terminal, files,
scheduler, and the other read-only ones), then the declared panel when the
record's `view.tui` is valid, then the generic panel. In a declared panel `g`
switches to the generic reflect view and back, `r` asks every section again, and
`j`/`k`/PageUp/PageDown scroll.

The panel only reads: it sends the declared verbs (when selected, on `r`, and on
`every_s`) and `reflect`, nothing else. A refusal shows in the section that asked,
as the kernel answered it: `refused: <error> (<reason>)`.

```yaml
# .fantastic/bundles/status_board/bundle.yaml
base: py_agent
sentence: A status board.
command: [python3, agent.py]
verbs: {status: "...", jobs: "...", events: "...", notes: "..."}
record:
  view:
    kind: html              # the web view
    bytes_file: panel.html
    calls: [status, jobs, events, notes]
    tui:                    # the terminal view
      - title: Status
        call: {verb: status}
        show: kv
        fields: [state, {path: load.1m, label: load}]
      - title: Jobs
        call: {verb: jobs, args: {limit: 20}}
        show: table
        fields: [id, {path: next.in_s, label: next}]
        every_s: 5
      - title: Events
        call: {verb: events}
        show: log
        every_s: 2
      - title: Notes
        call: {verb: notes}
        show: text
```

## Share it

A bundle is a folder of open files: zip it, send it, drop it into another
project's `.fantastic/bundles/`; read it and adapt it before running it.

## Trust

A python agent's code runs with your OS rights: what it does itself (processes,
files, network) passes no kernel gate. Run code you have read.
