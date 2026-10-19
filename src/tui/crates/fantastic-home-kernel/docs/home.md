# home

The manager of your fantastics: every fantastic folder the registry
(`<home>/fantastics/<folder>/fantastic.yaml`) lists, with its status, and the
folders you open. It lives in the home kernel, whose folder is `<home>/app`
(its workdir and its state dir: `lock.yaml`, `agent.yaml`, `agents/…`,
`home.db`, `web/…` and the run file `home.yaml`, with no `.fantastic` inside),
which only the desktop app hosts. Each fantastic's launch log is
`<home>/logs/<folder>-<hash8>.log`, on every OS.

## Calling it

Every client calls it the way the home page does: `user.forward` with
`target: "home"`, over `ws://127.0.0.1:<port>/user/ws`. Every intent replies
with the state right after it; what its effects change arrives later as `state`
payloads.

| verb | args | what |
|---|---|---|
| `list` | — | the state |
| `open` | `paths: [str]` | open each folder's page; start a stopped one; add an unlisted one |
| `register` | `path: str` | add a folder the list does not hold |
| `start` / `stop` / `remove` | `id: str` | when the row's `actions` hold the verb |
| `select` | `id: str \| null` | show a row's page, or the home |
| `select_page` | `index: int` | show the open page at `index` |
| `close_page` | `id: str` | close an open page |
| `dismiss` | — | clear the problem |
| `log` | `id: str` | `{id, file, tail}`: the launch log, in `<home>/logs/` |
| `focus` | — | emits `{"type":"focus"}` for the host shell |
| `reflect` | — | `{id, sentence, protocol, version, mode, home, poll_ms}` |

## The state stream

Watch `home` (dial `ws://127.0.0.1:<port>/home/ws`): every change is emitted
as `{"type":"state", "protocol":"fantastic.home", "version":1, "seq", …}`. A
watcher also sees the calls other clients send to `home`, mirrored as they
were sent, so it keeps only the payload `type` it wants. Watch first, then call
`list`, and ignore any `state` whose `seq` is not above the `list` reply's.

## What it keeps

`pages` and `selection` are fields of this agent's own record, written once
the home has listed the registry, and restored at `boot`. A page reopens only
while its registry entry still exists. The home's own folder, `<home>` itself, is never a row.
