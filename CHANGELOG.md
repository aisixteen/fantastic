# Changelog

All notable changes to `fantastic-canvas`. This project is pre-1.0 (alpha);
the wire protocol and on-disk `.fantastic/` format may change between releases.

## [Unreleased]

### Added

- File and folder agents: thin view agents over the file bridge (#150, Python
  reference). `open_view {kind: file|folder, path}` on a `file_bridge` returns the
  view of that path, making it if none exists, so one path is one agent. A file
  view reads, writes (`if_match`) and watches its file (`changed`); a folder view
  lists, reveals a path inside it and watches its entries (`added`, `changed`,
  `removed`). A view reaches its bridge as itself, for its own path only: its
  grant names the bridge's verbs it needs, and a path outside its scope is refused.
  The Rust `fantastic-file` port matches it; Swift has none yet. The canvas and
  CLI views of these agents are not in this entry yet.
- A file's or folder's disk change is judged by the disk as it is now: a batch that
  reports a removal and a spurious creation for one unlink reads as `deleted`
  (Python and Rust `file_bridge` watchers).
- Drag from the file tree onto the canvas (#134, #135). The tree's rows drag
  together: Shift-click selects a range, ⌘-click (Ctrl-click off macOS) adds or
  drops one row. An agent row drops its cell back on the canvas (shown again, or
  moved when it is already on the view, never duplicated); a file drops a
  `preview` pinned to it; a folder drops a file tree rooted at it. Several items
  land around the drop point, one slot each. The view's new `open {path | agent,
  x?, y?}` verb does the same for an AI. The drop highlight is the skin's accent
  tint. A tree can start at any folder (`root` on its cell). E90 `tree_drag`
  drives a drop with a DataTransfer (CDP drags are not reliable). Clicking a
  row with ⌘/Ctrl no longer focuses the agent on its canvas: it toggles the
  selection.
- A live file tree, a previewer and an editor on the canvas, replacing the agent
  tree. The `file tree` cell shows the project folder (`.fantastic` highlighted;
  `.git`, `node_modules`, `target`, `dist`, `__pycache__` hidden), lists a folder
  when it opens and watches it while open, so files made, changed or removed on
  disk show without a reload; every agent is a folder in it, with its badge, status
  and name. What the old agent tree did on a plain click (view a page agent, show or
  open a host agent's cell) is now a double-click, ↗ or Shift+Enter; a click selects. The `preview`
  cell follows the tree's selection or is pinned to a path: highlighted text
  (followed live), rendered Markdown, images, PDF, a size card for a binary file,
  paged `read_stream` for a large one, and an agent's own live view. Text files up
  to 1 MiB can be edited and saved with ⌘S / Ctrl-S against the version read; a
  change on disk under unsaved edits is a loud conflict (reload, keep mine,
  compare), never merged, never overwritten silently. Both are page agents with
  verbs an AI drives them by (`select`, `expand`, `collapse`, `reveal`; `open`,
  `pin`, `unpin`, `mode`, `edit`, `save`, `revert`). New skin tokens
  `color.syntax.*` and `color.tree.fantastic`. (#106)
- The view's `terminal_renderer {name?: auto | webgl | dom}`: `auto` draws
  terminals with xterm's DOM renderer in WebKitGTK (the app on Linux, GNOME
  Web) — known by the shell's flag or the user agent — and with the shared
  WebGL renderer everywhere else; switching is live. The e2e probe E88
  measures keystroke → render latency per renderer, in Chrome and, on Linux,
  in WebKitGTK.
- Installers for the desktop app, built and verified by CI (`installers.yml`,
  from the binaries a green CI run tested) and signed by hand by the
  maintainer: a notarized DMG (macOS, Apple silicon), a signed NSIS installer
  (Windows x64), and a `.deb` with a GPG signature (Linux x86_64: Ubuntu
  24.04+, Debian 13+), attached to the GitHub Release;
  `installers.yml -f check=vX.Y.Z` verifies the published ones. No AppImage:
  distro WebKitGTK can't be relocated without patching it; a Flatpak is
  planned (#82). `release.yml` names its CI run in the notes.
- The desktop app (`src/app`, Tauri 2): it hosts the home kernel in-process and
  shows its Home page in one window, with a native backdrop (Liquid Glass or
  vibrancy on macOS, Mica on Windows 11), native menus and shortcuts from the
  home's menu spec, and Open Folder, drops and Finder/Dock opens as intents. One
  app per home; quitting shuts the kernel down in order and leaves the canvases
  it started running. `--kiosk` runs it fullscreen with no decorations, for the
  kiosk.
- The `home` agent's `menu` and `focus` verbs and its `menu_spec` stream.
- The `fantastic-home` crate: the fantastic manager's model and ViewModel, pure
  (no OS, no I/O, no clock), the base of `fantastic home`.
- `fantastic-home-kernel`: the manager of your fantastics as a library the
  desktop app hosts in-process. A minimal kernel in the fantastic home dir (its
  own sqlite store, a loopback web host, no exec, files or AI) whose `home`
  agent lists, opens, starts, stops, registers and removes fantastics, keeps its
  open pages and selection across restarts, never lists its own folder, emits
  its state and serves a list page. Every OS call is in `home/sys`. A running
  row stays running through missed canvas answers until the 3rd in a row
  (about 6 s), so a loaded machine does not drop every open page's canvas.
- `fantastic __ctrl_break <pid>` (Windows): a hidden helper that stops a canvas
  running in another console.
- Python kernel: `sqlite_store`, a durable memory store. It answers exactly what
  `ram_store` answers, from one SQLite file, so an AI agent's conversation and
  folds, the execution ledger, dead letters and named-call dedup survive a
  restart with no dump and inject. Same schema as the CLI's rust store.
- `contract/memory_store/cases.json`: shared memory-store cases that python and
  rust replay against both of their stores.
- AI agents: `context_forget` compacts an agent's own store. It deletes the rows
  its folds already summarise and every superseded `view` row (with
  `include_cleared`, everything before the last `clear` as well), keeps every
  fold, and leaves the next prompt unchanged.
- Skins: the canvas draws every colour, radius, font family, shadow and motion
  value from design tokens (W3C 2025.10), compiled in the page. A `skin` page
  agent picks the skin per backdrop and takes skins an AI writes (`schema`,
  `get`, `add`, `use`). Two stock skins, `dark` (today's look) and `glass` (for a
  native vibrancy backdrop). `fantastic readme skins` documents the schema.
- The Home screen the home kernel serves, as a canvas view bundle (`home_view`):
  your fantastics with live status, start, stop, open and remove; one live page
  per open fantastic, kept across restarts; a nav pill and the shortcuts mod+0,
  mod+1–9 and mod+W; Open Folder through the shell or a path field. The Home page
  keeps its own records in `<home>/app/`.
- The reskin contract: the `home` protocol v1, documented in
  `fantastic-home-kernel/docs/home.md` and served by `docs of=home`;
  `contract/home/v1/cases.json`, replayed by the Rust agent and the TS client;
  READMEs for Rust shells of `fantastic-home` and `fantastic-home-kernel`; a
  one-file example client at `list.html`. A non-Rust shell talks to a running
  app's port.

### Changed

- CI runs only by hand: a push to a `fix/**` or `feat/**` branch no longer
  starts the units. Ready branches are batched into one temporary
  `ci/batch-<n>` branch and run once.
- `home` refuses a client asking for a protocol version other than 1.
- The home kernel serves the Home at `home.html` and the list page at
  `list.html`.
- On Windows, canvases start in a hidden console of their own and stop through
  a short-lived helper, so no console window flashes and a canvas from an
  earlier session stops too.
- Home layout v2: everything fantastic keeps for a user lives under ONE
  directory, `~/.fantastic/` (`FANTASTIC_HOME` still overrides it), in groups:
  `app/` (the desktop app's home kernel), `fantastics/` (the registry),
  `user/setups/ai_agent/` (saved AI set-ups, was
  `~/.config/fantastic/agents/ai_agent/saved/`), `user/keys/` (was
  `~/.config/fantastic/agents/ai_agent/keys/`), `user/credentials/` (the
  credential_store `file` default in python, rust and swift, was
  `~/.fantastic/credentials/`) and `logs/` (canvas launch logs on every OS, was
  `~/Library/Logs/Fantastic`, `$XDG_STATE_HOME/fantastic/logs` or
  `%LOCALAPPDATA%\fantastic\logs`). The home kernel's folder is `<home>/app`,
  its workdir and its state dir (was `<home>/.fantastic/app`, with its state
  in `<home>/.fantastic/app/.fantastic`). The home was `~/.config/fantastic`.
  Nothing moves by itself: `fantastic migrate-home` moves an older layout's
  files (`--dry-run` prints the moves), and refuses while a fantastic or the app
  runs or a target exists.
- Every runtime's disk bootstrap refuses a workdir whose `.fantastic` is the
  fantastic home: `<dir>: its .fantastic is the fantastic home (<home>) — run
  fantastic in a project folder`.
- Rust kernel: the state dir is a boot option, `StorageMode::Disk { workdir,
  state_dir }`. `BootstrapOptions::daemon` and `one_shot` keep
  `<workdir>/.fantastic`; `daemon_at(workdir, state_dir)` names another. The
  on-disk format is unchanged.
- The canvas theme panel uses the canvas palette and gains a Skins section. AI
  agents made before this release call the skin agent once their cell is
  reopened.
- CI: a failed unit suite stops the run before the e2e. The e2e runs only once
  every unit job passed and every binary built; the builds still run alongside
  the unit tests (`build.yml`'s `binaries` call, then its `e2e` call).
- CI runs the unit suites only by default (a bug fix, titled `ci · units`). The
  release binaries and the e2e run on request (`-f e2e=true`, titled
  `ci · full`): for a completed phase or a release. `release.yml` publishes only
  a green full run.
- CI: a push to a `fix/**` or `feat/**` branch runs the unit suites by itself
  (`ci · units`); a newer push to the same branch cancels the older run. Every
  other run is still started by hand.

### Fixed

- S4: the desktop app's shell flag (`__FANTASTIC_SHELL__`) reaches the canvas
  on macOS and Linux too: it is set in every frame, so the canvas (a frame of
  the Home page) knows its webview and backdrop; only the top frame gets
  `pickFolder`. Before, Tauri's main-frame script reached subframes on Windows
  alone.
- S3: the desktop app's opaque window (Linux; Windows without Mica) no longer
  flashes light on open: the window and webview start in the dark skin's page
  colour, so the boot page's transparent body is never the webview's white.
- exec_bridge (python, rust, swift): a monitor always ends with one delivery
  headed `job ended`, even when nothing waited any more, and `monitor` refuses a
  job whose end already went out (flaky on Windows, #64).
- The lockfile no longer pins the yanked `yoke-derive 0.8.3` (now 0.8.4).
- AI agents: fold summaries no longer leave the view once the next prompt is
  rebuilt from the view array. Each new fold records its place (`anchor`), so it
  is placed even when its members are not in the rows being rendered; a store
  written before the field renders as it did.
- AI agents (rust): a fold whose place cannot be known no longer panics inside
  the turn. It fails the turn loud, as python does: `status done` with
  `reason: error` and the message, then a raised `ValueError`. Swift and rust
  now print the offending id the way python does.
- e2e `canvas.ai_cell` S8 (#74): the step waits for the cell's fold notice
  (`context folded`) and answers the `context_recall` side call in its own shape.
  It used to wait for `context compacted`, which only showed because a fold
  dropped out of the next view (the bug above). With that fixed, every S8
  compaction is a fold, and a recall made due by the folds took a turn's queued
  mock reply.
- The desktop app on Linux shows its canvases again: its window's navigation
  filter admits the running canvases' ports. WebKitGTK sends their frames
  through the same check, which refused them (a regression from #77).
- home: a canvas that fails to start says why. The row reads `it did not start
  — <error>`, the launch log keeps the error, and `--kiosk` prints a new problem
  once on stderr.
- Canvas terminal: after a re-attach, flow control counts each character once,
  so the unacked count no longer reads low and output is not sent past the
  limit.

## [0.0.6] - 2026-10-18

The model-free e2e is green on Linux, macOS and Windows CI runners — 0.0.5's
known issue is closed. The AI cell's pill staying on "thinking" after a stop
(the other 0.0.5 known issue) was the test mock's race, not the cell.

### Changed

- README: release, crates.io, platform and roadmap badges, and a Roadmap section
  (the public board and its four milestones); no links to private repositories.
- CONTRIBUTING: outside contributions are not accepted; bug reports are welcome
  as issues, security problems through private vulnerability reporting.

### Fixed

- canvas terminal: a ▶ start no longer drops what the program wrote before
  the start's reply named its job (a shell's first prompt on a slow host) —
  it is drawn and acked, as an attach already did.
- canvas terminal: keys reach the program in the order typed — one `write` in
  flight at a time, what is typed meanwhile sent next, whole. The host runs
  each call as its own task, so a line and its Enter could arrive swapped.
- `fantastic canvas` dev watcher: a spawned bundle's folder is watched BEFORE
  it loads, and its own `added` event reloads nothing — that event, late on
  Windows, restarted the instance the spawn had just made, mid-call. A
  hand-made folder, or one made again after its removal, still loads.
- Windows realpath: a file replaced or deleted while it is looked at (an
  atomic save over `agent.yaml`, a cell's folder removed) resolves to the
  volume's `\$Extend\$Deleted\` — taken as missing now, so its parent is
  resolved and the name joined on; and the `\\?\` the OS hands back is dropped
  unless the path without it resolves elsewhere. A record being saved or
  forgotten "escaped" its root (`persist_record` / `forget_record … escapes
  root`, canvas cells under load). A refused clamp now prints both paths.
- `fantastic` key store: storing a key waits for `user`'s grant on a store
  made a moment ago (the first key, its mode just chosen) — a loaded machine
  refused it (`user may not call 'set' on credentials`).
- e2e: the reference dev surface serves with its own `serve.py` (`-m
  http.server` reverse-resolves its address before it says it listens — a
  macOS runner stalled there); a TUI shot waits for a settled screen (300 ms
  unchanged, at most 3 s — ConPTY hands a redraw over in pieces); a click
  waits for its target to be SHOWN, not only present (a stopped terminal's ▶
  is in the DOM hidden until its attach); the AI-cell mock answers the
  runtime's no-call reminder `done` at once, so it never takes the next step's
  reply (S4, S5, S11); prompt_file checks the turn that answers its message; a
  cell deleted between a listing and its read is skipped; the Swift
  scheduler-batch test waits for its fires to be recorded before its directory
  is removed (it left `fantastic-schedbatch-*` behind).
- e2e: a cold Chrome is given 30 s to attach; the reference surface's log
  panels keep each job's lines (a first line could land before the job was
  named to them) and its status follows the run's own job (a build's late
  exit read as the run stopping); the hover-cost check compares the best of
  three interleaved samples (one each measured a shared runner's noise).
- CI: the Windows e2e runs one test at a time; a failed canvas test prints
  every page wait that ran out, when, and where the host's log stood.

## [0.0.5] - 2026-10-17

### Added

- Windows (x86_64): the `fantastic` CLI, the rust kernel and the python kernel
  run on Windows. Every OS call lives in the crate's (package's) `sys` module,
  one file per OS family: the workdir lock (`LockFileEx`), owner-only files
  (a protected DACL), exec_bridge jobs (Job Objects, ConPTY terminals, a
  peer-verified loopback control socket), the console, Ctrl-Break as SIGTERM.
- GitHub Release archives for five targets: macOS arm64 + x86_64, Linux musl
  x86_64 + arm64, and `fantastic-x86_64-pc-windows-msvc.zip`, with
  `SHA256SUMS`.
- CI (`.github/workflows/ci.yml`, started by hand — `gh workflow run ci.yml
  --ref <branch>`): every runtime's unit tests on Linux, macOS and Windows, the
  five release archives, and the model-free e2e on those very binaries, each
  OS's suite as three parallel shards. `release.yml` (by hand, on a `vX.Y.Z`
  tag) publishes the archives of a green CI run on the tagged commit.
- e2e: `E2E_ARTIFACTS=<dir>` runs the suite on prebuilt binaries
  (`e2e/build.py` writes them); unset, it builds from source. `--shard i/n`
  runs one slice of the selection.
- README: for an AI agent — which archive to pull, how to verify it, and doc
  mode (`fantastic` off a terminal, `fantastic readme [key]`).
- user home store: `register_home_folder` / `name_home_folder` keep a
  fantastic's folder in `<home>/fantastics/`; `fantastic canvas` writes
  `.fantastic/cli/canvas.yaml` while it serves.

### Changed

- exec_bridge (python, rust, swift): a PTY `start` with neither `command` nor
  `args` runs the record's whole `shell` line (`$SHELL -l` under
  `shell: "login"`); the canvas no longer sends `-l`.
- exec_bridge: the OS's required environment (`SYSTEMROOT` on Windows) reaches
  every job, whatever `inherit_env` says.
- A loader's reflected `root` and every path on the wire are `/`-separated on
  every OS.

### Fixed

- The part splitter's stop is terminal in every runtime (a chunk fed after a
  stop opened a new part).
- python: on Windows, stdin is a terminal only if it is a console (`NUL` read
  as a tty and started a REPL).
- rust: a dangling link's target resolves through its existing part (an 8.3
  short path compared unequal with its long form).
- rust (Windows): a path another thread was saving at the same moment kept its
  `\\?\` prefix and was refused as escaping its root (a canvas save lost).
- canvas: a terminal re-attached after a page reload keeps its one-row shrink
  at least 150 ms, so a full-screen program on Windows (ConPTY) repaints.

### Known issues

- The model-free e2e is not yet green on CI's runners: on Windows several
  canvas probes time out (page, terminal and wizard steps run ~50× slower than
  on a desktop), and a few probes race the slower Linux/macOS runners too. Unit
  tests pass on all three OSes, and every archive is built by CI.
- canvas: an AI cell stopped under load can keep its phase pill on "thinking"
  after the stop (the turn is stopped; the next message answers).


## [0.0.4] - 2026-10-16

### Added

- crates.io package `fantastic` (`cargo install fantastic --locked`), generated by
  `scripts/build/crate.sh`: the CLI and every crate it links as modules of one
  package, with the canvas page embedded prebuilt (no Go at install time).
- ts kernel: secret marks on send items (call rows and inbox mirrors show
  `[secret]`; the ws bridge carries the marks both ways).
- `ram_store` (python, rust, swift): an (owner, name) index, so a fresh
  execution name no longer scans every stored call.
- e2e: the no-model `kernel/` group (E110–E122) on python, rust and swift;
  canvas runners use `$FANTASTIC_BIN` when set.
- exec_bridge (python, rust, swift): a monitor whose `notify` refuses a delivery
  logs it and emits `monitor_dead`.
- cli: `sys` module, terminal zoom click and input box, cell min/max.

### Fixed

- rust drain: a dropped caller's future settles its ledger entry `cancelled`.
- scheduler (python, rust, swift): history appends are serialized per scheduler.
- swift `<call>` parser: searches bounded to the call being parsed (no longer
  quadratic over the completion).
- cli: a panel edit redraws its cells (views carry the panel's content hash).
- exec_bridge (python, rust, swift): an `output` replay clears the PTY ack
  backlog, so a reloaded viewer no longer leaves a terminal paused for good.
- persistence (python, rust, swift): removing an agent before its first write no
  longer deletes its directory.
- swift: the on-device model's context size is read once (it blocked the pool);
  stream part assembly is linear.
- cli: terminals draw only PTY bytes.

### Removed

- The GitHub Actions workflows.

### Changed

- Docs and comments: general prose only (no dated tags, attributions, tracking
  ids or downstream project names).

## [0.0.3] - 2026-10-15

First release of the `fantastic` CLI: prebuilt binaries for macOS (arm64,
x86_64) and static Linux (musl, arm64, x86_64) with `SHA256SUMS`;
`cargo install --git` from a clean checkout. The repository moved to
`aisixteen/fantastic`.

### Added — kernel and protocol (python, rust, swift, ts)

- Array-only batch `send` with per-item outcomes, unified refusals and an
  execution ledger: caller-named executions, `retries`, dead letters
  (`ledger`, `ack_deadletter`), correlation PIDs and one int64 id space.
- Call security: a two-sided, deny-all call gate (`can_call` + `callable_by`),
  identity from dispatch only, `$self` / `$parent` / `$subtree` grant keys, the
  read side (`watch`, `state_subscribe`) gated; the minimal access model
  replaced identity switches and secret machinery.
- Memory stores: the kernel keeps no conversation; every message and execution
  row lives in a memory store agent (`ram_store`; `sqlite_store` in the CLI).
  The row state event announces each row, gated per reader.
- `docs` verb (oneliner and detailed tiers), per-instance verb docs, root
  `env`/`version`/`runtime` reflect fields, kernel-instance guid,
  `kernel_start` / `shutdown_agents` / `kernel_stop` lifecycle verbs.
- Folder bundles (`bundles/<name>/bundle.yaml`) with the `py_agent` base and
  `reload_bundle` / `read_bundles`.
- Declarative `static_loader`; `credential_store` (secrets by ref; file,
  keychain, env backends); secret values marked on a call, kept by no record.
- `mcp_client`: a remote MCP server as an agent, its tools as per-instance
  verbs (protocol 2026-07-28 plus the initialize-era versions).
- `exec_bridge`: the one process edge — commands and PTY jobs, `$SHELL`
  discovery, monitors (early stop of long jobs), tail/grep of a job's output.
- `file_bridge`: sealed by default with a deny-all `allow` list and running-dir
  clamp; text verbs with `patch_text_file` as an atomic multi-file transaction;
  `read_stream` / `write_stream` / `pump` over raw-byte frames.
- Bridges: `cloud_bridge` replaced by `relay_connector` (relay-kernel router,
  `X-Fantastic-Auth`); io_bridge converged across all four runtimes; one binary
  body per frame.
- web: loopback bind with an Origin guard; event-driven hosts and schedulers
  (no idle wakeups).

### Added — AI (`ai_core` and backends)

- RAW prompt-and-parse tool calling with `<call>` tags (no JSON on the model
  path, no provider tool API); a call checker with compiler-style reports;
  calls inside thinking do not dispatch.
- Backends: `vllm_backend`, `responses_backend` (OpenAI Responses API),
  `anthropic_backend`, `foundation_models_backend` (swift FMCore, linked into
  rust through a C ABI), alongside `ollama_backend`.
- Context protocol: compaction by folds sized by context share, `context_recall`
  folding an over-budget head, `context_clear`, a fixed fold-summary budget.
- Human-loop `inquiry` between agents; system prompts from a file through a
  `file_bridge_id`; record-level `system_prompt`; token usage on `done`.

### Added — CLI and canvas

- The `fantastic` CLI runs the rust kernel in process; `fantastic mcp` is the
  coding-CLI entry point (`.fantastic/.mcp.json`); `fantastic send` / `readme`.
- The browser canvas replaced the browser View: AI agent tiles, terminal cells
  (xterm WebGL through one shared context), themes, the AI set-up wizard,
  saved set-ups, headless `fantastic canvas`.

### Changed

- Kernels reorganized under `src/lib/*`; the kernel host binary is
  `fantastic_kernel`.
- The ts library is the base kernel only; the canvas page lives in the CLI
  project.
- Shared contract corpus (`contract/`) and shared e2e scenarios run on python,
  rust and swift.

### Removed

- `durable_memory` (memory is plain files through `text_file`), `apple_kvs`,
  the swift UniFFI compat surface, `ledger.json`, the rust/swift `get` system
  verb, base64 on the wire, the unreleased container images.

## [0.5.5] - 2026-06-06

First release under AGPL-3.0-or-later.

### Changed — License

- **Relicensed from Apache-2.0 → AGPL-3.0-or-later.** Both this repo and the
  Apple client ([`fantastic_app`](https://github.com/Alexadar/fantastic_app))
  are now AGPL-3.0-or-later — one product family, same author. The trademark
  carve-out is retained (now framed under AGPL §7): the "Aisixteen Fantastic" /
  **AISIXTEEN** marks are not licensed, so a fork must rename.
- **This binds future versions only.** Releases already published under
  Apache-2.0 — **v0.4.0, v0.5.0, v0.5.1, v0.5.2, v0.5.3, v0.5.4** — remain
  Apache-2.0 for anyone who received them. The relicense applies from the next
  release onward.

### Added

- Community-health files: `SECURITY.md`, `CONTRIBUTING.md`, `NOTICE`,
  `CITATION.cff`, issue + PR templates.

### Removed — Backward-compat (alpha; no BC kept)

- The `return_readme` reflect alias (all four runtimes + producers); `readme`
  is the sole flag. Cross-runtime reflect parity stays byte-identical.
- Duplicate AI `say` events (per-tool-call + 429 retry path); structured
  `status` events stay. The TS-consumed `queued`/`done` events are kept.

---

> Earlier releases (v0.4.0 – v0.5.4) predate this changelog; see the GitHub
> Releases page and git history.
