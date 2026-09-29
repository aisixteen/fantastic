> If you are `claude` in a PTY job of an `exec_bridge` on this host.

# If you are a terminal here

You may be `claude` running as a PTY job of an `exec_bridge` agent on this host
(a terminal is a job: `start pty={cols,rows}`). The view rendering you lives in the
browser frontend, not the agent tree — reflect yourself and walk `parent_id` up to
see what you're under. To add FRONTEND views beside yourself (any frontend view
record — `handler_module` ending in `.ts`), `persist_record` them to `web_loader`
(see `readme=frontend/panel-recipe`); to add HOST agents, `create_agent` here.
(Lost your own id? `$FANTASTIC_SELF_ID` names the exec_bridge agent whose job you
are; `status` on it lists its jobs.)

To learn ANY agent: reflect it with `readme=true`.
