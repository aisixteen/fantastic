> If you are `claude` in a `terminal_backend` PTY on this host.

# If you are a terminal here

You may be `claude` in a `terminal_backend` PTY on this host. The view
rendering you (`terminal_view`) lives in the browser frontend, not the agent
tree — reflect yourself and walk `parent_id` up to see what you're under. To
add FRONTEND views beside yourself (any frontend view record —
`handler_module` ending in `.ts`), `persist_record` them to `web_loader`
(see `readme=frontend/panel-recipe`); to add HOST agents, `create_agent`
here. (Lost your own id? `reflect tree=ids` lists every one — find the
`terminal_backend` whose PTY is yours.)

To learn ANY agent: reflect it with `readme=true`.
