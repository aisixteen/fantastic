# Aisixteen Fantastic

*A medium that unifies humans and AIs into a single workspace.*

[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)
![status: alpha](https://img.shields.io/badge/status-alpha-orange)

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

## Security posture

Nothing is open by default. Every IO leg denies until an operator opens it,
grants are explicit on both ends of a call, an agent never raises its own
authority, and file access is clamped to the directory the kernel runs in. One
root `reflect` shows the whole landscape — which legs are open, what each agent
may call. Report issues privately: see [`SECURITY.md`](SECURITY.md).

<!-- CLI SECTION — written and kept current by the CLI session; the lib side does not edit between these markers. -->

## The `fantastic` CLI

`fantastic` runs the Rust kernel in-process in the current directory (`.fantastic/`):
a terminal coder, a browser **canvas** where you build, run and engineer your own
systems visually with an AI, and an MCP face for coding CLIs.

```sh
cargo install --path src/tui    # builds `fantastic` (Rust toolchain; the canvas is packed into the binary)
```

### Start with your coding agent

Open your favourite CLI agent (Claude Code, Codex, Gemini CLI, …) in a project
folder and paste:

> Run `fantastic` and read what it prints. Explain to me what it is, then ask me
> how I want to run it.

Run without a terminal, as an agent's shell runs it, `fantastic` is in **doc
mode**: it prints its readme and the command list, and starts and writes nothing.
`fantastic readme [key]` opens any doc (`canvas`, `mcp`, `setup`, `bundles`, and
the kernel's own). From there the agent can start it the way you choose, or wire
itself in through `fantastic mcp`.

### Commands

```sh
fantastic                       # the terminal UI
fantastic canvas [--port N]     # the browser canvas, headless: the kernel's messages stream in this terminal
fantastic --headless            # no UI, no canvas: the message stream only
fantastic ai vllm -p PORT       # the root AI on a backend (also ollama -p PORT, responses|anthropic --model M)
fantastic run CMD [ARGS]        # your own program in a terminal (with canvas: a terminal cell)
fantastic mcp                   # MCP over stdio for a coding CLI (attaches to a running kernel, or runs it)
fantastic readme [key]          # the docs
```

One kernel runs per folder; a bare `fantastic` started there while it runs attaches to it.

<!-- /CLI SECTION -->

## Runtimes

Three interchangeable hosts share one wire protocol and one `.fantastic/` workdir
(one active per workdir), plus a browser frontend:

- [`src/lib/python/`](src/lib/python/): the reference; when runtimes disagree, Python is right
- [`src/lib/rust/`](src/lib/rust/): server and CLI host
- [`src/lib/swift/`](src/lib/swift/): Apple platforms, linked in-process by the
  [Apple client](https://github.com/Alexadar/fantastic_app)
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

## Read more

- [`src/lib/python/CLAUDE.md`](src/lib/python/CLAUDE.md) — the protocol reference:
  architecture, every bundle, the universal patterns, storage
- [`THESIS.md`](THESIS.md) and [`paper/`](paper/00_index.md) — the why
- [`docs/`](docs/) — the IO bridge specs
- [`CHANGELOG.md`](CHANGELOG.md) · [`CONTRIBUTING.md`](CONTRIBUTING.md) ·
  [`CITATION.cff`](CITATION.cff)

## License & brand

AGPL-3.0-or-later ([`LICENSE`](LICENSE)). This repo, the
[Apple client](https://github.com/Alexadar/fantastic_app) and the
[relay](https://github.com/Alexadar/fantastic_relay) are one product family by
Koreniuk Oleksandr (aisixteen, kvazis@gmail.com).

**Trademarks (AGPL §7):** **AISIXTEEN®** (U.S. Reg. No. 7,238,635; Int'l Reg. No.
1720602) is a registered trademark of Koreniuk Oleksandr Vasylovych; "Aisixteen
Fantastic" is a trademark of the same owner. The license grants no rights in these
marks, so a fork must ship under a different name.
