> The one discovery verb, its flags, and how to explore without flooding context.

# The reflect surface

`reflect` is the one discovery verb. It returns the ADDRESSED agent
uniformly — `{id, sentence, display_name, description?, verbs?, ...state}`.
Three things are root-only: the deployment fields (`runtime`/`env`/`version`),
`peers` — the kernels this one is connected to, `[{guid, via}]`, each agent there
reachable as `<guid>|<agent id>` (reflect `<guid>|kernel` for its tree) — and the
ANSWERING of `kernel_verbs` (advertised everywhere, answered by the root alone). Otherwise the root is not special. Compose the reply with three
flags:

- `tree=all|ids|none` (default `all`)
  - `all`  — the agent's whole subtree, nested + distilled
             (`{id, parent_id, handler_module, display_name, description?, children}`).
  - `ids`  — a flat list of descendant ids. Cheap. Use it to scan.
  - `none` — just the agent, no subtree.
- `bundles=all|ids|none` (default `none`)
  - `all`  — every installable bundle as `{name, handler_module}`, plus `schema`
    where the bundle publishes one: what that KIND's record takes, readable before
    any agent of it exists — `{fields:[{name, type, required, enum?, default?, hint}],
    key_var}` (`type` is `str|int|float|bool|enum|list|map`; `default` is the
    bundle's own value and never present on a required field; `key_var` is the
    environment variable the key is read from, `null` if none). The same schema is
    what `create_agent`/`update_agent` enforce: a required field missing is refused
    at the write, every one named with its hint.
  - `ids`  — just the bundle names.
  - `none` — omit (the usual case).
- `readme=true|index|<key>` (default `false`) — the manual, one agent at a
  time. `true` = the short orientation; `index` = the keyed topic map
  (`{key, desc, tokens, children}`); `<key>` = one topic's body + its
  sub-docs' index. Unknown key → `{error, keys:[…]}`.

## Context hygiene

Reading reflect output is cheap; readmes are not. To explore without
flooding your context: SCAN with `reflect tree=ids bundles=ids` (ids only),
then DRILL into ONE id at a time with `reflect tree=all` or `reflect
readme=<key>`. Never pull every agent's readme at once — fetch the single
doc for the topic you are about to act on.

## The `description` field

Any agent may carry a short `description` — a one-line "what this agent is
for", set at create or update:

    create_agent parent_id=<owner> handler_module=<bundle>.tools description="..."
    update_agent id=<agent> description="..."

It surfaces in every reflect (top-level and in each tree node), so a single
`reflect tree=all` tells you what every agent in the tree does at a glance —
no per-agent readme needed just for the overview.
