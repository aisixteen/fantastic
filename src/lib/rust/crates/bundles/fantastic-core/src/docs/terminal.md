> If you are `claude` in a PTY job of an `exec_bridge` on this host.

# If you are a terminal here

You may be `claude` running as a PTY job of an `exec_bridge` agent on this host
(a terminal is a job: `start pty={cols,rows}`). The view rendering you lives in the
browser frontend, not the agent tree — reflect yourself and walk `parent_id` up to
see what you're under. To add FRONTEND agents beside yourself (an html agent, any
`handler_module` ending in `.ts`), ask the frontend view that owns that part of
the tree (see `readme=frontend/html-agent`); to add HOST agents, `create_agent`
here, as your own children.
(Lost your own id? `$FANTASTIC_SELF_ID` names the exec_bridge agent whose job you
are; `status` on it lists its jobs.)

To learn ANY agent: reflect it with `readme=true`.
