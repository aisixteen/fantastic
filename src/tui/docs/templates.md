> Plugin templates: a host agent with its own view, saved once, placed from the canvas's double-click menu.

# Templates

A TEMPLATE is one record saved in your home (`<home>/templates/<name>.yaml`;
`<home>` is `FANTASTIC_HOME`, else `~/.config/fantastic`). Two kinds:

- an **AI template** (its handler makes an AI — the set-up page writes these):
  the menu's `host · AI` group opens a chat on it;
- a **plugin template** (any other handler): the menu's `host` group places it.

## A plugin template

The host agent's record, with its own VIEW (backend-owned views):

```yaml
# <home>/templates/web_search.yaml
handler_module: exec_bridge.tools
cwd: .
ingress_rule: allow_all_ingress
allowed: ['^python3 search\.py \S+$']
view:
  kind: html                       # or terminal | chat | stub, or handler: <a page bundle>
  bytes: "<button id=go>search</button><pre id=out></pre><script>…</script>"
  calls: [start, status, watch]    # the verbs the panel calls on its agent
```

Placing it makes a COPY under `canvas_root` (`<name>_<hex>`, `from: <name>`) and
a cell that VIEWS it: the body stays on the agent's record, the cell's grant is
derived from `calls`. The panel's JS uses `fantastic.send/watch` on its agent.
No id, memory store or grants in a template — placing adds them; a key is named
by reference (`credentials`), never inline.

Editing a placed copy changes only that copy (`update_agent` on it, or its
files); re-saving it as a template is its own act.

## Saving one

Only the human saves templates (the set-up page, or a file in
`<home>/templates/`): the home store serves the CLI alone, so no agent — an AI,
or a record in a cloned project — can rewrite the user's templates.
