# Aisixteen Fantastic

*A medium that unifies humans and AIs into a single workspace.*

[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)
![status: alpha](https://img.shields.io/badge/status-alpha-orange)
[![release](https://img.shields.io/github/v/release/aisixteen/fantastic)](https://github.com/aisixteen/fantastic/releases/latest)
[![crates.io](https://img.shields.io/crates/v/fantastic)](https://crates.io/crates/fantastic)
![platforms: macOS · Linux · Windows](https://img.shields.io/badge/platforms-macOS%20%C2%B7%20Linux%20%C2%B7%20Windows-lightgrey)
[![roadmap](https://img.shields.io/badge/roadmap-app%20%E2%86%92%20fantastic%20OS-5319e7)](https://github.com/orgs/aisixteen/projects/1)

![Aisixteen Fantastic](media/hero.gif)

Fantastic is a kernel of **agents**. A model, a program, a file tree, a shell, a
browser canvas and the human at the keyboard are all agents in one tree, addressed
by id and reached the same way: one call, `send`. Compute, inference and memory are
interchangeable — a step written as code and the same step written as an LLM call
plug into the wiring identically.

There is no client library. **The protocol is the API**: every agent answers
`{"type":"reflect"}` with what it is, and `docs` says how to call it. Given only
those self-descriptions, an AI wires the system itself — that is the bar the
model-driven test suite holds it to.

## The idea in one call

Every interaction is one batch send — a list of items, each `{target, payload}`,
answered in order with `{pid, status, result}`:

```json
[{"target": "kernel", "payload": {"type": "reflect", "readme": true}}]
```

That one reflect returns the live tree, the kernel's manual index and, with
`bundles: "all"`, every kind of agent you can create — enough for a person or a
model to take it from there with `create_agent`, `send` and `docs`.

| | |
|---|---|
| **Agents** | a recursive tree; each runs a bundle (a handler module) and keeps its state in its own record |
| **`send`** | the one primitive: batched, concurrent, each item a kernel task you can `ps` and `cancel`, joinable and idempotent by id |
| **`reflect` / `docs`** | what an agent is / how to call it — the whole self-description, one round-trip each |
| **The call gate** | deny-all, two-sided: a call passes only when the caller's `can_call` and the target's `callable_by` both grant it |
| **Bridges** | every edge to the outside — another kernel, the disk, a process, a browser, an MCP server — is an IO leg, sealed until opened |
| **Memory** | every message lives in a memory store agent; AI agents fold, recall and page their own history |

## What's in the box

- **AI agents** over vLLM, Ollama, the OpenAI Responses API and Anthropic's
  Messages API (plus Apple's on-device model in Swift). Tool calling is the
  kernel's own prompt-taught `<call>` grammar — no provider tool API — so any
  capable model drives the same verbs.
- **IO legs**: `file_bridge` (files, edits, streams, watches; clamped to the
  running dir, deny-all at the path level), `exec_bridge` (allow-listed commands
  and PTY terminals as kernel tasks), `web_ws` / `web_rest` (the browser and HTTP
  faces), `ws_bridge` / `relay_connector` (kernel to kernel), `mcp_client` (a
  remote MCP server's tools as verbs).
- **Folder bundles**: drop `bundles/<name>/bundle.yaml` plus a program into
  `.fantastic/` and it becomes a new kind of agent, hot-reloadable.
- **Kernel services**: a scheduler, credentials by reference (keys bound to one
  host; a send can mark payload fields `secret`, so records and events show them
  as `[secret]`), a persistent tree on
  disk under `.fantastic/`, and a task table with Unix-style `ps` / `cancel`.

## Fantastic and other agent harnesses

What agent frameworks and coding harnesses typically offer as of October 2026
(LangGraph, Microsoft Agent Framework, OpenAI Agents SDK, Claude Agent SDK, Google
ADK, CrewAI, Mastra, Pydantic AI, Letta, Claude Code, Codex, Gemini CLI and
others), and what Fantastic does instead. "Not built" means exactly that.

| Typical in other harnesses | Fantastic |
|---|---|
| **Agent loop**: a while-loop per agent (model → tools → repeat until a step cap), or a hand-built state graph | No loop object. A model pass is one message; a chain continues only when the model sends — to itself or to others. All calls of a turn dispatch at once as one kernel batch, and their replies come back later as one message: flat asynchronous flows between addressable agents, mailbox-style. A chain ends only by the model's own `done`; silent turns are capped and thought-only turns are rerolled |
| **Tool calling**: the provider's native function-calling API with JSON schemas | One prompt-taught `<call>` tag grammar, no JSON on the model path, the same on every provider and local model. A malformed call gets a compiler-style report (line, caret, the form to write); a call written inside the model's thinking never runs |
| **Tool discovery**: a fixed tool list in the prompt; skill files for progressive disclosure | Every agent answers `reflect` (what it is) and `docs` (how to call it, one-liners or detailed). A model drills down on demand, and docs it already read shrink to stubs |
| **Parallel tool calls** in one turn, when the provider supports them | Every send is a batch. One turn of 500 calls runs as one kernel batch on every runtime |
| **Multi-agent**: handoffs, supervisor, swarm, agent-as-tool, sub-agents with their own context | Every AI is an agent with an id: AI-to-AI sends, fan-out to many recipients, inquiries between AIs. Models build their own durable teams — sub-AIs, workers, schedules — and tear them down. No prebuilt supervisor or handoff presets |
| **Graphs and workflows**: an explicit state-graph or workflow DSL with conditional edges, cycles and fan-out | No graph DSL. The graph is the agent tree plus its grant edges, and a node can be code, a model, a process or a file store — interchangeably. Models wire graphs from the agents' own self-descriptions; a scheduler drives recurring flows |
| **Durable state**: a per-thread checkpointer; crash-resume through a durable execution engine (Temporal, DBOS, Restate, Inngest) that logs every step and replays to the point of the crash | A kernel ledger: every call and its outcome is a row in a memory-store agent. A named execution is idempotent — a repeat joins it or gets its recorded outcome — with retries and dead letters (listed, acked, never silently re-sent). The agent tree, schedules and conversations are persisted and rehydrate. Not built: replaying an interrupted turn to where it stopped — an interrupted call becomes a dead letter |
| **Checkpoints and time travel**: replay or fork a run from a past checkpoint; coding CLIs rewind edits through a shadow git repo | The ledger already holds every conversation row and every tree change, so both are a projection away from rewind and fork. Not built: the rewind verb, and the record of replaced file bytes — today one undo slot per edited file |
| **Human in the loop**: interrupt and resume; approve, edit or reject a tool call; permission modes | The human is an agent (`user`). A model asks through `inquiry` (text, single, multi, confirm) or a plain send and continues when answered; nothing blocks the kernel. Approval splits the rights: a model without the grant proposes a named action, the person who holds it approves, and it runs exactly once under the approver's authority. Not built: turning a denied call into an approval question automatically |
| **Memory**: thread state, a long-term store with vector search, self-edited memory blocks, instruction files | Every message lives in a memory-store agent (RAM, or SQLite with full-text search in the CLI), per agent, searchable with `grep` and `recall`. Durable notes are files the model writes through a file agent. No vector search, by design: similarity is approximate, and a memory read should return exactly what was written |
| **Context management**: summarize or compact at a threshold; tool-result clearing | Nested folds: the model summarizes spans of its own history, then folds of folds. Nothing is deleted — `recall`, `grep` and descending into a fold reach everything; `clear` is a boundary, not a deletion. The budget is calibrated from the server's own prompt-size reports |
| **Streaming and cancellation**: token and event streams, an abort signal | Streamed parts (text, thinking, call drafts) to whoever watches; the receiver can stop a stream. Every dispatch is a kernel task: `ps` lists them, `cancel` kills a whole subtree, real OS processes included. Timeouts are per item and never defaulted |
| **Security**: approval modes, hooks, OS sandboxes (Seatbelt, bubblewrap, containers, microVMs), guardrail classifiers | A deny-all, two-sided call gate on every agent-to-agent call; every IO leg sealed until opened; file access clamped to the run directory with an allow list; commands allow-listed by regex; secret marks; credentials by reference, bound to one host. That governs what may be launched; containing a launched process is not built — no OS sandbox, so an allowed command runs with the user's rights |
| **Protocols**: MCP client (universal), MCP server, A2A, AG-UI, ACP | MCP client (2026-07-28; each remote tool becomes a verb), an MCP server face for coding CLIs (`fantastic mcp`, stdio), its own WebSocket and REST wire, and kernel-to-kernel bridges, direct or through a relay. No A2A, AG-UI or ACP |
| **Observability**: OpenTelemetry traces, hosted tracing, eval suites, visual debuggers | A live state stream of who sent what to whom, the ledger, the task table, and the canvas views over them. No OpenTelemetry export; evaluation is the e2e suites and an Exercism benchmark |
| **Background work**: hosted agent runtimes, cron triggers, cloud background agents | A scheduler (interval-based, persisted, fires under its own grants), command and terminal jobs as kernel tasks, folder-bundle programs as agents. No cron expressions, no hosted platform |
| **Runtimes**: mostly Python and TypeScript; some .NET, Go, Java | One protocol in Python, Rust and Swift (in-process on Apple platforms) plus a browser kernel in TypeScript; a single static binary; a contract corpus pins identical behaviour across them |
| **Models**: provider SDKs; other providers through adapters | vLLM, Ollama, the OpenAI Responses API, Anthropic, and Apple's on-device model — the same call grammar on all; a KV-cache slot table for shared vLLM servers |
| **UI**: chat UI kits, generative UI, IDE extensions, studios | A terminal UI and a browser canvas where a model builds live panels, terminals and services and wires them by grants |
| **Extensibility**: plugins, skill files, tools written in code | Folder bundles — a `bundle.yaml` plus a program becomes a new kind of agent, hot-reloaded, self-describing like every other |

## Security posture

Nothing is open by default. Every IO leg denies until an operator opens it,
grants are explicit on both ends of a call, an agent never raises its own
authority, and file access is clamped to the directory the kernel runs in. One
root `reflect` shows the whole landscape — which legs are open, what each agent
may call. Report issues privately: see [`SECURITY.md`](SECURITY.md).

## Install

The `fantastic` CLI runs on **macOS, Linux** (arm64 and x86_64) **and Windows**
(x86_64). Its AI
backends are vLLM, Ollama, the OpenAI Responses API and Anthropic.

### For an AI agent: pull, verify, read

Every release carries one archive per platform and a `SHA256SUMS`, at
`https://github.com/aisixteen/fantastic/releases/latest/download/<archive>`:

| this machine (`uname -sm`, or Windows) | archive | binary inside |
|---|---|---|
| `Darwin arm64` (Apple silicon) | `fantastic-aarch64-apple-darwin.tar.gz` | `fantastic` |
| `Darwin x86_64` (Intel Mac) | `fantastic-x86_64-apple-darwin.tar.gz` | `fantastic` |
| `Linux x86_64` | `fantastic-x86_64-unknown-linux-musl.tar.gz` (static) | `fantastic` |
| `Linux aarch64` | `fantastic-aarch64-unknown-linux-musl.tar.gz` (static) | `fantastic` |
| Windows x86_64 | `fantastic-x86_64-pc-windows-msvc.zip` | `fantastic.exe` |

macOS / Linux:

```sh
A=fantastic-aarch64-apple-darwin.tar.gz          # the row for this machine
U=https://github.com/aisixteen/fantastic/releases/latest/download
curl -fsSLO "$U/$A" && curl -fsSLO "$U/SHA256SUMS"
grep " $A\$" SHA256SUMS | shasum -a 256 -c -     # Linux: sha256sum -c -
tar xzf "$A"                                      # → ./fantastic
```

Windows (PowerShell):

```powershell
$A = "fantastic-x86_64-pc-windows-msvc.zip"
$U = "https://github.com/aisixteen/fantastic/releases/latest/download"
Invoke-WebRequest "$U/$A" -OutFile $A; Invoke-WebRequest "$U/SHA256SUMS" -OutFile SHA256SUMS
(Get-FileHash $A -Algorithm SHA256).Hash.ToLower() -eq ((Select-String " $A$" SHA256SUMS).Line -split " ")[0]
Expand-Archive $A -DestinationPath .               # → .\fantastic.exe
```

Then READ before running anything — **doc mode** starts nothing and writes nothing:

```sh
./fantastic                  # stdout not a terminal (an agent's shell): the readme + every command
./fantastic readme           # the docs index, one line per doc
./fantastic readme mcp       # one doc: canvas, mcp, setup, bundles, and the kernel's own
```

From there, run the mode the user asks for (see [Commands](#commands)): `fantastic mcp`
to drive it as an MCP server, `fantastic canvas` for the browser canvas,
`fantastic --headless` for the message stream alone. It lives in the directory it is
started in (`.fantastic/`) and nowhere else.

### Prebuilt binary

Download the archive for your platform from the
[latest release](https://github.com/aisixteen/fantastic/releases/latest) and put
`fantastic` on your `PATH`. On a Mac with Apple silicon:

```sh
curl -LO https://github.com/aisixteen/fantastic/releases/latest/download/fantastic-aarch64-apple-darwin.tar.gz
tar xzf fantastic-aarch64-apple-darwin.tar.gz && sudo mv fantastic /usr/local/bin/
```

The archives are `fantastic-<target>.tar.gz` for `aarch64-apple-darwin`,
`x86_64-apple-darwin`, `aarch64-unknown-linux-musl` and
`x86_64-unknown-linux-musl` (static Linux binaries), plus
`fantastic-x86_64-pc-windows-msvc.zip`, with `SHA256SUMS` beside them.

### Desktop app

The desktop app (`fantastic-app`) hosts the home kernel and shows your
fantastics in one window; the `fantastic` CLI ships inside it. Its installers are
on the same [release](https://github.com/aisixteen/fantastic/releases/latest),
beside `SHA256SUMS-app`:

| system | installer |
|---|---|
| macOS 14+, Apple silicon | `fantastic-app-aarch64-apple-darwin.dmg` (notarized) |
| Windows 10/11 x64 | `fantastic-app-x86_64-pc-windows-msvc-setup.exe` (per user, no admin) |
| x86_64 Linux: Ubuntu 24.04+, Debian 13+ | `fantastic-app-x86_64-unknown-linux-gnu.deb` (`sudo apt install ./…deb`) |

The `.deb` installs `/usr/bin/fantastic-app` and `/usr/bin/fantastic`, with
`Depends: libwebkit2gtk-4.1-0, libgtk-3-0` and `Recommends: fonts-dejavu-core`.
On a minimal system installed with `--no-install-recommends` (the kiosk), add
`fonts-dejavu-core` and `libgl1-mesa-dri`; `fantastic-app --kiosk` then runs it
fullscreen, with no desktop environment. The session must set `SHELL`, as a
login does: the app starts each canvas in the login environment `$SHELL` prints.

Other Linux distributions: a Flatpak is planned
([#82](https://github.com/aisixteen/fantastic/issues/82)). There is no AppImage:
the distributions' WebKitGTK finds its helper processes at a path compiled into
the library, so an AppImage works only by patching that library.

Verify before you install:

```sh
spctl -a -vvv -t open --context context:primary-signature fantastic-app-aarch64-apple-darwin.dmg   # macOS: source=Notarized Developer ID
gpg --import fantastic-app-linux-signing-key.asc && gpg --verify fantastic-app-x86_64-unknown-linux-gnu.deb.asc fantastic-app-x86_64-unknown-linux-gnu.deb
```

```powershell
Get-AuthenticodeSignature .\fantastic-app-x86_64-pc-windows-msvc-setup.exe   # Status: Valid
```

The Linux key's fingerprint is the one in [`SECURITY.md`](SECURITY.md#verifying-downloads).
CI builds and tests the installers; the maintainer signs them by hand.

### With cargo

From crates.io:

```sh
cargo install fantastic --locked
```

This needs Rust ≥ 1.88 and a C compiler. The package carries the canvas page
prebuilt, so no Go is needed. Its default features are `vllm`, `ollama`,
`responses` and `anthropic`; pick a subset with
`--no-default-features --features <list>` (with none, the binary has no AI backend).

From git:

```sh
cargo install --locked --git https://github.com/aisixteen/fantastic fantastic-tui \
  --features vllm,ollama,responses,anthropic
```

This builds the source tree, which packs the canvas page at build time, so it also
needs **Go ≥ 1.21**: the page is packed with a pinned esbuild, which the build
compiles once from pinned source and verifies by sha256
([`src/lib/ts/tools/esbuild/`](src/lib/ts/tools/esbuild/README.md)).
Drop `--features` you don't need; with none, the binary has no AI backend.

### Run from source

```sh
git clone https://github.com/aisixteen/fantastic && cd fantastic/src
cargo run --release -p fantastic-tui --features vllm,ollama,responses,anthropic -- readme
```

Everything after `--` goes to `fantastic` (same requirements as `cargo install`).
The kernel hosts run on their own too:

```sh
cd src/lib/python && uv sync && uv run fantastic_kernel   # the Python reference kernel
cd src/lib/rust   && cargo run --bin fantastic_kernel      # the Rust kernel host
cd src/lib/swift  && swift run fantastic_kernel            # the Swift kernel (macOS 14+)
```

Each runtime's build, test and lint commands are in [`CONTRIBUTING.md`](CONTRIBUTING.md).

<!-- CLI SECTION — written and kept current by the CLI session; the lib side does not edit between these markers. -->

## The `fantastic` CLI

`fantastic` is one static binary that runs the Rust kernel in-process, in the
current directory. Everything it keeps lives in `.fantastic/`, and nothing is
created outside it. It gives you a terminal UI, a browser **canvas** where you
build, run and wire systems visually with an AI, and an MCP face for coding CLIs.
One kernel runs per folder: a second `fantastic` started there attaches to the
running one.

```sh
cargo install --path src/tui    # builds `fantastic`; the canvas page is packed into the binary
```

### Start with your coding agent

Open your CLI agent (Claude Code, Codex, Gemini CLI, …) in a project folder and
paste:

> Run `fantastic` and read what it prints. Explain to me what it is, then ask me
> how I want to run it.

Without a terminal, as an agent's shell runs it, `fantastic` is in **doc mode**:
it prints its readme and the command list, and starts and writes nothing.
`fantastic readme [key]` prints any doc: `canvas`, `mcp`, `setup`, `bundles`, and
the kernel's own.

### Commands

```sh
fantastic                         # the terminal UI; a fresh folder is seeded on launch
fantastic canvas [--port N]       # the browser canvas, headless; the kernel's message stream on stdout
fantastic --headless              # no UI, no canvas: the message stream only
fantastic [canvas] ai BACKEND     # the root AI `ai`: vllm -p PORT | ollama -p PORT | responses|anthropic --model M
fantastic [canvas] run CMD [ARGS] # your own program from $PATH in a terminal (with canvas: a terminal cell)
fantastic mcp                     # MCP over stdio (tools `reflect`, `send`); attaches to a running kernel, or runs it
fantastic send '<item json>'      # one call into the running kernel; `$env:NAME` values come from this shell, marked secret
fantastic migrate-home [--dry-run] # move an older layout's files into ~/.fantastic
fantastic readme [key]            # the docs
```

These words are **recipes**: each adds its agents only if they are missing, then
runs them. `--port` is recorded on the first `canvas` and reused after that.

### The canvas

The page is its own kernel in the browser, bridged to the host kernel through the
`canvas_ws` leg. It holds no memory and no AI: every cell is a **view** of a host
agent (a terminal, an AI chat, a python agent, an MCP server), or page-side
content (`html`, `gl`).

- **Double-click** opens the selector, which lists everything that can be placed
  there: `AI agent`, a terminal, coding agents found on your `PATH` (`claude`,
  `codex`, `aider`, `opencode`, `fantastic`), `python agent`, `mcp connector`,
  `tree view`, `canvas theme`, `user store`, `tips`, and your bundles. Nothing
  floats on the canvas; every surface is a cell.
- **AI agents** are set up in a two-page wizard: the provider (responses,
  anthropic, ollama or vllm), then its name, URL, model and key. Saved set-ups
  live in your home folder, and the ★ one is the default. Nothing is guessed:
  there are no default endpoints, and keys are never read from the environment.
- **Terminals** are `exec_bridge` PTY jobs running your login `$SHELL`, drawn by
  WebGL through one shared GPU context, so hundreds of them stay smooth. Nothing
  starts on its own: after a restart a terminal shows as stopped until you press
  `▶`. A terminal re-attaches to its live job after a reload and redraws.
- **html cells** run sandboxed (null origin) unless `trusted: true`. Their only
  way out is the connector: `fantastic.send`, `emit`, `onMessage`, `watch`. An AI
  builds them with the view's verbs `place`, `update`, `fit`, `move`, `wire`
  and `remove`.
- **Canvases** are flat peers. `new_canvas {name}` makes one, and
  `?canvas=<node id>` opens it. Each has its own page kernel and shows only its
  own subtree. **Themes** (free canvas, one column, your own) move cells and
  never the agents.
- **Grants**: `user`, and every AI under a canvas, may call every host agent
  with every verb (god mode, granted on both sides). You narrow it with
  `update_agent`.
- **The system block**: `.fantastic/prompt.md` is every AI's system prompt. Edit
  it, and every AI uses your text from its next turn.

### Agents, bundles and keys

- **Folder bundles**: `.fantastic/bundles/<name>/` holds `bundle.yaml` (its
  verbs), `agent.py` and `panel.html` (its live view). A save restarts every
  instance, and a panel edit re-renders every cell. Page bundles
  (`.fantastic/web/bundles/<name>/bundle.yaml`, `base: html_agent`) are reusable
  html cells. An AI writes a bundle only when you ask for one; otherwise it makes
  agents with `create_agent` from the bundles `reflect` lists.
- **Keys** live in the `credentials` agent, under a ref `<provider>/<host>` that
  is bound to that host. Storage is the human's choice: plain (one 0600 file) or
  secure (the keychain). Your home is `FANTASTIC_HOME`, or
  `~/.fantastic`, and holds saved set-ups, key refs and the list of your
  fantastics (see [Where fantastic keeps your files](#where-fantastic-keeps-your-files)).
- **Coding CLIs** connect through `.fantastic/.mcp.json`, which every start
  writes (`claude --mcp-config .fantastic/.mcp.json`). The client is the `mcp`
  agent, and each tool call is a kernel task that `ps` lists and MCP
  cancellation stops.

### Where fantastic keeps your files

Everything fantastic keeps for you lives under ONE directory, the home:
`FANTASTIC_HOME`, else `~/.fantastic` (`%USERPROFILE%\.fantastic` on Windows).
Each project keeps its own state in its own `.fantastic/`.

```
~/.fantastic/                    <home>: FANTASTIC_HOME, else ~/.fantastic
├─ app/                          the desktop app's home kernel, its state dir
│   ├─ lock.yaml  agent.yaml     one host per home; guid, next_id
│   ├─ home.db                   its sqlite store
│   ├─ home.yaml                 run file {pid, port, url}
│   ├─ agents/…                  host records (the home agent's pages + selection)
│   └─ web/…                     the Home page's records
├─ fantastics/<guid>/fantastic.yaml   the registry of your fantastics
├─ user/
│   ├─ setups/ai_agent/…         saved AI set-ups
│   ├─ keys/…                    key mode + refs
│   └─ credentials/<id>.yaml     a credential_store's `file` backend default
└─ logs/<folder>-<hash8>.log     canvas launch logs, every OS

<project>/.fantastic/            each fantastic's own state
```

No `.fantastic` is nested in the home, so a fantastic is never run in `~`
itself: its `.fantastic` would be the home, and every runtime refuses that.

**Moving from an older layout.** Earlier versions kept these files in
`~/.config/fantastic/` (the registry, `agents/ai_agent/saved/` and
`agents/ai_agent/keys/`) and `~/.fantastic/credentials/`. Nothing moves by
itself; stop every fantastic and the app, then:

```sh
fantastic migrate-home --dry-run   # print every move
fantastic migrate-home             # move them
```

It refuses, and moves nothing, when a target already exists, a fantastic is
running, or the app is open. Each file keeps its permissions.

### How the CLI is tested

- Unit suites: `cargo test -p fantastic-tui`, and the page's `npm test` in
  `src/tui/web/canvas`.
- Model-driven e2e in [`src/tui/e2e/`](src/tui/e2e/tests/README.md): a real
  headless canvas in Chrome, driven by a live model or by CPU-only probes
  (terminal zoom clicks, typing, redraw after a reload, PTY backpressure). Each
  run is scored by what it produced.

<!-- /CLI SECTION -->

## Runtimes

Three interchangeable hosts share one wire protocol and one `.fantastic/` workdir
(one active per workdir), plus a browser frontend:

- [`src/lib/python/`](src/lib/python/): the reference; when runtimes disagree, Python is right
- [`src/lib/rust/`](src/lib/rust/): server and CLI host
- [`src/lib/swift/`](src/lib/swift/): Apple platforms (its retirement, keeping the
  Foundation Models bridge, is [on the roadmap](https://github.com/aisixteen/fantastic/issues/53))
- [`src/lib/ts/`](src/lib/ts/): the browser frontend kernel, served by any host

A change to the protocol, the on-disk format or `reflect` lands in every host, and
the shared [`contract/`](contract/) corpus pins identical behaviour across them.

## How it is tested

- **Unit suites** in each runtime, run in parallel (build, test and lint commands
  per runtime: [`CONTRIBUTING.md`](CONTRIBUTING.md)).
- **The contract corpus** ([`contract/`](contract/)): cross-runtime requirements
  written as data, run by every host.
- **Model-driven e2e** ([`e2e/`](e2e/README.md)): live models build, code, fold
  their memory and wire agents through the real kernels, scored by what they
  produced — not by what they said.

## Roadmap

Public, on [the board](https://github.com/orgs/aisixteen/projects/1) and in
[#57](https://github.com/aisixteen/fantastic/issues/57): from the app to fantastic OS.

| Milestone | What it brings |
|---|---|
| [M1 — App MVP](https://github.com/aisixteen/fantastic/milestone/1) | The manager of your fantastics as a desktop app: Home lists them with live status; open, start and stop them |
| [M2 — Cross-platform app and the reskin contract](https://github.com/aisixteen/fantastic/milestone/2) | Tauri on macOS, Windows and Linux with signed installers; one UI, many skins, and the contract other projects reskin |
| [M3 — fantastic OS: kiosk](https://github.com/aisixteen/fantastic/milestone/3) | A Debian headless image that boots straight into fantastic |
| [M4 — fantastic OS: system agents](https://github.com/aisixteen/fantastic/milestone/4) | Network, audio, power and displays as agents |

## Read more

- [`src/lib/python/CLAUDE.md`](src/lib/python/CLAUDE.md) — the protocol reference:
  architecture, every bundle, the universal patterns, storage
- [`THESIS.md`](THESIS.md) and [`paper/`](paper/00_index.md) — the why
- [`docs/`](docs/) — the IO bridge specs
- [`CHANGELOG.md`](CHANGELOG.md) · [`CONTRIBUTING.md`](CONTRIBUTING.md) ·
  [`CITATION.cff`](CITATION.cff)

## License & brand

AGPL-3.0-or-later ([`LICENSE`](LICENSE)). This repo, the Apple client and the relay
(both in development) are one product family by
Koreniuk Oleksandr (aisixteen, kvazis@gmail.com).

**Trademarks (AGPL §7):** **AISIXTEEN®** (U.S. Reg. No. 7,238,635; Int'l Reg. No.
1720602) is a registered trademark of Koreniuk Oleksandr Vasylovych; "Aisixteen
Fantastic" is a trademark of the same owner. The license grants no rights in these
marks, so a fork must ship under a different name.
