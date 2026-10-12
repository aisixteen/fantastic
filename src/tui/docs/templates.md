> AI templates: your saved AI set-ups — the AI agent's provider, model and key by reference.

# Templates

A TEMPLATE is one AI set-up saved in your home
(`<home>/agents/ai_agent/templates/<name>.yaml`; `<home>` is `FANTASTIC_HOME`,
else `~/.config/fantastic`): an AI backend's `handler_module` and its config.
The set-up page writes them; `.preferred` names the default (★).

They are not menu tiles. The double-click's `AI agent` opens a chat on the
default; another set-up is chosen in the AI cell's ⚙ settings → `‹ other
set-ups`. A template on a handler that is not an AI is refused.

No id, memory store or grants in a template — placing adds them; a key is named
by reference (`credentials`), never inline.

## Saving one

Only the human saves templates (the set-up page, or a file in
`<home>/agents/ai_agent/templates/`): the home store serves the CLI alone, so no agent — an AI,
or a record in a cloned project — can rewrite the user's templates.
