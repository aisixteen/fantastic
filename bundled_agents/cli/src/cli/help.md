# fantastic — CLI

A Fantastic kernel is a tree of agents. One primitive:
`send(target_id, payload) -> reply`. Every agent answers
`{"type":"reflect"}`.

## Invocations

    fantastic                          interactive REPL (tty) and/or
                                       daemon (if a web agent is persisted)
    fantastic <id> <verb> [k=v ...]    one-shot RPC — print JSON, exit
    fantastic reflect [<id>] [k=v]     one-shot: <id> reflect (default 'kernel')
    fantastic install <dir> [pkg ...]  uv venv <dir>/.venv + install
    fantastic install-bundle <spec>    uv pip install a fantastic bundle
    fantastic --help                   this file

## Discover the system

    fantastic reflect                       the live agent tree + bundles
    fantastic reflect return_readme=true     ...plus the bootstrap readme
    fantastic reflect <id> return_readme=true  any agent + its readme

`reflect` is read-only and lock-free — it works whether or not a
daemon owns the dir. Every other one-shot (`<id> <verb>`, `install`)
acquires the PID lock and is **refused while a daemon is running** —
go through that daemon's web surface instead (a `web` agent's
`web_rest` / `web_ws` children; see `fantastic reflect`).

## Run a daemon

There is no `--port` flag. Persist a web agent first, then boot:

    fantastic core create_agent handler_module=web.tools port=8888
    fantastic <web_id> create_agent handler_module=web_ws.tools
    fantastic <web_id> create_agent handler_module=web_rest.tools
    fantastic                          # rehydrates + serves

Start with `fantastic reflect return_readme=true` — it tells you
everything else.
