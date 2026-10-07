> The one discovery verb, its flags, and how to explore without flooding context.

# The reflect surface

`reflect` is the one discovery verb. It returns the ADDRESSED agent
uniformly — `{id, sentence, display_name, description?, ...state}` — what it IS,
never how to call it: verbs, emits and cancel are `docs` (send `kernel`
`{type: docs, of: <id>}`). Two things are root-only: the deployment fields
(`runtime`/`env`/`version`) and `peers` — the kernels this one is connected to,
`[{guid, via}]`, each agent there reachable as `<guid>|<agent id>` (reflect
`<guid>|kernel` for its tree). Otherwise the root is not special. Compose the reply
with three flags:

- `tree=all|ids|none` (default `all`)
  - `all`  — the agent's whole subtree, nested + distilled
             (`{id, parent_id, handler_module, display_name, description?, children}`).
  - `ids`  — a flat list of descendant ids. Cheap. Use it to scan.
  - `none` — just the agent, no subtree.
  - with `all` or `ids`: `show_subtrees=[ids]` answers with ONLY those branches;
    `ignore_subtrees=[ids]` prunes each named branch to `{id, pruned: n}` (n = the
    agents it holds), so you see something is there and can open it later. Both:
    show first, then ignore inside it. An id outside the tree is refused `bad_arg`.
- `bundles=all|ids|none` (default `none`)
  - `all`  — every installable bundle as `{name, handler_module}`, plus `schema`
    where the bundle publishes one: what that KIND's record takes, readable before
    any agent of it exists — `{fields:[{name, type, required, enum?, default?, hint}]}`
    (`type` is `str|int|float|bool|enum|list|map|credentials`; `default` is the
    bundle's own value and never present on a required field; a `credentials` field
    is `{store, ref}` — where a backend reads its key, never the key). The same schema is
    what `create_agent`/`update_agent` enforce: a required field missing is refused
    at the write, every one named with its hint.
  - `ids`  — just the bundle names.
  - `none` — omit (the usual case).
- `readme=true|index|<key>` (default `false`) — the manual, one agent at a
  time. `true` = the short orientation; `index` = the keyed topic map
  (`{key, desc, tokens, children}`); `<key>` = one topic's body + its
  sub-docs' index. Unknown key → `{error, keys:[…]}`.

## Context hygiene

To explore without flooding your context: SCAN with `reflect tree=ids
bundles=ids` (ids only), then DRILL into ONE id at a time. Once you know which part
of the tree you work on, reflect with `show_subtrees` / `ignore_subtrees` — a
full-tree reflect is for the first look only. Pull `docs` once per kind of agent,
`detailed` only for the verb you are about to use. Never pull every agent's readme
at once — fetch the single doc for the topic you are about to act on.

## The `description` field

Any agent may carry a short `description` — a one-line "what this agent is
for", set at create or update:

    create_agent parent_id=<owner> handler_module=<bundle>.tools description="..."
    update_agent id=<agent> description="..."

It surfaces in every reflect (top-level and in each tree node), so a single
`reflect tree=all` tells you what every agent in the tree does at a glance —
no per-agent readme needed just for the overview.
