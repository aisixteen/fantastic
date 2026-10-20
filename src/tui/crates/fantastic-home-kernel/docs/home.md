> The `home` protocol v1 (`fantastic.home`): host it, connect, every verb, the state, versioning, and how to bring your own UI.

# home

The manager of your fantastics: every fantastic folder the registry
(`<home>/fantastics/<folder>/fantastic.yaml`) lists, with its status, and the
folders you open. It lives in the home kernel, whose folder is `<home>/app`
(its workdir and its state dir: `lock.yaml`, `agent.yaml`, `agents/…`,
`home.db`, `web/…` and the run file `home.yaml`, with no `.fantastic` inside).
Each fantastic's launch log is `<home>/logs/<folder>-<hash8>.log`, on every OS.

This file is the protocol reference. The kernel serves it as this agent's
readme and as its `protocol` docs topic; `docs of=home tier=detailed` serves
the same contract as data (every verb, the emits, the notes).
`contract/home/v1/cases.json` pins it: the Rust agent and the TS client both
replay it.

## 1. Host it

- The desktop app hosts the home in-process, and so does the kiosk, which runs
  the app. A Rust shell of its own links `fantastic-home-kernel` and calls
  `HomeKernel::boot` (section 8).
- One host per home dir: the host holds the folder lock of `<home>/app`. There
  is no standalone host and no `fantastic home` command.
- The host serves, on `http://127.0.0.1:<port>`:
  - `/canvas_page/file/home.html`, the Home;
  - `/canvas_page/file/list.html`, the example client (section 6).
- While a host serves, its run file `<home>/app/home.yaml` is
  `{pid, port, url}`. That is how any client finds the port. Trust it only while
  `pid` is alive and holds the lock (`running()` in the library does both).

## 2. Connect

**Calls** go over `ws://127.0.0.1:<port>/user/ws`, one frame each:

```json
{"type": "call", "target": "user", "id": "<fresh id>",
 "payload": {"type": "forward", "target": "home", "payload": {"type": "list", "version": 1}}}
```

The answer is `{"type": "reply", "id", "data"}`, `data` the verb's reply (a
refusal is a reply too: `data.error` set), or `{"type": "error", "id", …}`
when the call could not be carried. Use a fresh id for every call (a UUID): a
call that repeats an earlier id joins that call and gets its answer. A frame
with no id (the socket's refused auto-watch of `user`) answers no call.

**Events** come over `ws://127.0.0.1:<port>/home/ws`: every payload on `home`,
as `{"type": "event", "payload": {…}}`. Its payloads are:

- `state`, on every change (section 4);
- `focus`, `{"type":"focus"}`, from the `focus` verb, for the host shell;
- `menu_spec`, `{"type":"menu_spec", seq, spec}`, after a state emit that
  changed the native menus (see `menu`);
- every call another client sent to `home`, mirrored as it was sent.

Keep only the payload types you read (a page keeps `state`). The order that
loses nothing: open `/home/ws` first, then call `list`, then draw only a
`state` whose `seq` is above the last one you drew.

**The `Origin` rule.** The host refuses a websocket whose browser `Origin` is
not one of its own (`http://127.0.0.1:<port>` and its loopback spellings) with
403. A client that sends no `Origin` header, which is any native client, is
admitted. The home's `web` record has no `allowed_origins`, and nothing a
client can reach adds one.

**What the socket reaches.** The socket's leg may call only `user.forward`
and watch `home`. Through `user` it reaches `home`'s verbs (below), the root's
`reflect` and `docs`, and `web_loader`'s three record verbs (`load_tree`,
`persist_record`, `forget_record`, which the page loader sends). Nothing else:
no tree verb (`create_agent`, `update_agent`, …), no file agent, no store. So
no client can widen a grant, serve another file or admit another origin.

## 3. Verbs

Every verb takes an optional `version: int` (absent means 1; section 5). Every
intent replies with the state right after it; what its effects change arrives
later as `state` payloads. A refused call changes nothing. A refusal is
`{error, reason, arg?, hint?}`, `reason` one of `missing_arg` (a required
argument is absent), `bad_arg` (present but wrong), `not_found` (no such row,
page or log) and `unhandled` (no such verb; it carries `verb`).

Every verb refuses `bad_arg` with `arg: "version"` for a version other than 1.

### `list`

No arguments. Returns the state.

### `open`

`paths: [str]`, required, at least one. Each is a listed row's `path` as it is,
or an absolute folder. Opens each folder's page and shows the last one; a
stopped one is started; an unlisted one is started and added to the list (its
row is `adding:<path>` until the registry lists it). Returns the state.
Refuses `missing_arg` (`paths` absent or empty) and `bad_arg` (`paths` not a
list of strings; a path that is not absolute, not a folder, or the home's own
folder).

### `register`

`path: str`, required: an absolute folder the list does not hold. Adds it, as
`open` does. Returns the state. Refuses `missing_arg` and `bad_arg` (not a
string, not absolute, not a folder, the home's own folder, or listed already).

### `start`

`id: str`, required: a row whose `actions` hold `start`. Starts
`fantastic canvas` in its folder; the row is `starting` until its canvas
answers (30 s grace, then the problem says so). Returns the state. Refuses
`missing_arg`, `not_found` (no such row) and `bad_arg` (its actions lack
`start`; the text names its status).

### `stop`

`id: str`, required: a row whose `actions` hold `stop`. Asks the process
holding its folder to stop; the row is `stopping` once asked. Returns the
state. Refuses `missing_arg`, `not_found` and `bad_arg` (its actions lack
`stop`).

### `remove`

`id: str`, required: a registry row. Removes it from the list. Only a running
fantastic changes the list: a running row removes itself; a stopped one is
removed through any running one's kernel (none running: the problem says so).
Returns the state. Refuses `missing_arg`, `not_found` and `bad_arg` (its
actions lack `remove`: a folder being added).

### `select`

`id: str | null`; the key is required. Shows row `id`'s page (opened if it is
not), or the Home for `null`. Returns the state. Refuses `missing_arg`,
`not_found` and `bad_arg` (neither a string nor null).

### `select_page`

`index: int ≥ 0`, required. Shows the open page at `index`. Returns the state.
Refuses `missing_arg` and `bad_arg` (not an integer ≥ 0, or no page there).

### `close_page`

`id: str`, required: an open page. Closes it; its fantastic keeps running.
Returns the state. Refuses `missing_arg` and `not_found` (no such page open).

### `dismiss`

No arguments. Clears the problem. Returns the state.

### `log`

`id: str`, required. Returns `{id, file, tail}`: the row's launch log file (in
`<home>/logs/`) and its last lines (`tail` null when unreadable). Refuses
`missing_arg` and `not_found` (no such row, or no launch log).

### `focus`

No arguments. Emits `{"type":"focus"}` on `home` for the host shell, which
raises its window; nothing else changes. Returns the state.

### `menu`

No arguments. Returns `{"type":"menu_spec", seq, spec}`: the native menus a
host shell builds, `seq` the state's. A host shell builds its menus from `spec`
(`fantastic_home::menu::Menu`, version 1): the bar's menus, each item an
`action` (a click sends its intent, one of this agent's verbs; `pick: "folders"`
asks the OS for folders first and sends them as `open`'s `paths`), a native
`role`, a `separator` or a nested `submenu`, each marked for the systems that
show it. After every state emit that changes it, the same `menu_spec` payload
is emitted on `home`. A page keeps only `state` payloads.

### `reflect`

No arguments. Returns `{id, sentence, protocol: "fantastic.home", version: 1,
mode, home, poll_ms}`: `home` the home dir, `poll_ms` how often it looks (2000).
Check `protocol` and `version` before anything else.

The kernel's own lifecycle verbs, `boot` (restores the kept pages and
selection, starts the poll) and `shutdown` (stops the poll), are never a
client's; `docs` lists them as lifecycle.

## 4. The state

What every intent answers and every change emits:

```json
{"type": "state", "protocol": "fantastic.home", "version": 1, "seq": 7, "mode": "lock",
 "listed": true,
 "rows": [{"id": "g-1", "guid": "g-1", "name": "notes", "path": "/home/me/notes",
           "last_seen": 1760800000, "status": "running",
           "canvas": {"url": "http://127.0.0.1:8123/canvas_page/file/index.html", "port": 8123},
           "pid": 4242, "actions": ["stop", "open", "remove"],
           "log": "/home/me/.fantastic/logs/notes-1a2b3c4d.log"}],
 "pages": ["g-1"], "selection": "g-1", "problem": null}
```

| field | type | what |
|---|---|---|
| `type` | `"state"` | |
| `protocol` | `"fantastic.home"` | |
| `version` | int | 1 |
| `seq` | int | rises by one with every change |
| `mode` | `"lock"` \| `"canvas"` | `lock`: the full app, which reads folders and locks; `canvas`: a sandboxed app, which may only ask canvases (nothing is started or stopped) |
| `listed` | bool | the registry has been looked at once; before that, `rows` is not the list yet |
| `rows` | list | one per registry entry, in registry order, then one per folder being added |
| `pages` | list of ids | the open pages, in order |
| `selection` | id \| null | the page shown; `null` is the Home |
| `problem` | str \| null | the last thing that went wrong, said for the user; `dismiss` clears it |

A row:

| field | type | what |
|---|---|---|
| `id` | str | its registry `guid`, or `adding:<path>` for a folder being added |
| `guid` | str \| null | null while being added |
| `name` | str | the entry's name, else its folder's last component |
| `path` | str | its folder, as a kernel running there names it |
| `last_seen` | int \| null | when it last ran, in seconds since the epoch; 0 when the entry never said; null while being added |
| `status` | str | below |
| `canvas` | `{url, port}` \| null | set while `running` |
| `pid` | int \| null | the process holding its folder, when known |
| `actions` | list | what may be asked of it now, below |
| `log` | str \| null | its launch log file, when it exists (`log` reads it) |

**The seven statuses.**

- `stopped`: nothing holds its folder.
- `starting`: the home started it and its canvas has not answered yet.
- `stopping`: the home asked it to stop and its folder is still held.
- `running`: the process holding its folder is the run its registry entry names,
  and its canvas answers as it (with its guid). A running row rides out missed
  canvas answers (a loaded machine misses the check's timeout): while the same
  process serves the same canvas, misses 1 and 2 in a row keep it `running`,
  and the 3rd shows what the check found, about 6 s at the 2 s poll. A gone
  process, another run or another canvas shows at once; an answer resets the
  count.
- `no_canvas`: it runs without a canvas (a terminal, or headless).
- `unverified`: something holds its folder, but it is not shown to be this
  entry's run, or its canvas does not answer as it.
- `missing`: its folder is gone, or no longer holds a fantastic.

**The actions**, in this order: `start` (lock mode, `stopped`) or else `stop`
(lock mode, a process holds it, not `stopping`); then `open` (always in lock
mode; in canvas mode unless it is a stopped entry); then `remove` (a registry
row). `start`, `stop` and `remove` are verbs taking the row's `id`; `open`
takes its `path` (`open {paths: [path]}`).

**Pages and selection are the home's.** They are fields of this agent's own
record, written once the home has listed the registry, and restored at `boot`:
they survive a restart, and every client sees the same. A page reopens only
while its registry entry still exists. The home's own folder, `<home>` itself,
is never a row.

## 5. Versioning

This is protocol v1. Every verb takes an optional `version`; absent means 1,
and any other is refused (`bad_arg`, `arg: "version"`, "home speaks
fantastic.home v1; this client asked for v2"). `reflect`, `list` and every
state carry `protocol` and `version`.

The compatibility rule:

- v1 never removes or renames a verb, an argument, a state field, an emitted
  payload type, a status or an action, and never changes a field's type or
  meaning.
- v1 may add an optional argument, a state field, a status-independent field,
  an emitted payload type or a verb. A v1 client ignores what it does not know,
  and keeps only the payload types it reads.
- Anything else is v2.

## 6. Bring your own UI

The ways a new look is made, and their limits:

- **Skin the served Home** with a token file (section 7). The Home, its
  behaviour and its OS layer stay as they are.
- **A Rust shell** links `fantastic-home-kernel`, boots the home in-process and
  shows the served Home in its own webview, or draws its own UI from `call` and
  `events`. A Rust shell with an OS layer of its own links only the pure
  `fantastic-home` and answers its questions itself (section 8).
- **A non-Rust shell** (Swift, for example) reads the run file and speaks this
  protocol over the running app's port. It sends no `Origin` header, so it is
  admitted. The limit: the app must be running, because there is no
  standalone host.
- **The example**, `list.html`, is the whole protocol in one file: read it to
  write a client. It runs where the home serves it,
  `http://127.0.0.1:<port>/canvas_page/file/list.html`. A copy opened from
  `file://` or from another origin is refused by the `Origin` rule, and the
  home can neither serve another file nor admit another origin: its socket holds
  no grant that could change either.

## 7. Skins

A skin is a W3C design token document compiled to CSS custom properties,
`--fc-<path, . as ->`. The schema, every token and the supported format are in
`src/tui/docs/skins.md` (`fantastic readme skins`; in code,
`src/tui/web/canvas/src/bundles/skin/schema.ts`). The reference skin is
`src/tui/web/canvas/src/bundles/skin/stock/dark.tokens.json`, and
`glass.tokens.json` beside it is the native one.

A page takes a skin as ONE `:root` block of the schema's custom properties,
and paints with `var(--fc-…)` only. `list.html` does exactly that, its block
copied from the dark skin: swapping that block is the whole reskin of the page.
The Home takes the skin agent's live skin the same way.

## 8. Rust shells

- `src/tui/crates/fantastic-home/README.md`: the pure model and ViewModel, for a
  shell with an OS layer of its own.
- `src/tui/crates/fantastic-home-kernel/README.md`: the library that hosts the
  home in-process (`HomeKernel`).
