# Aisixteen Fantastic

*A medium that unifies humans and AIs into a single workspace.*

[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)
![status: alpha](https://img.shields.io/badge/status-alpha-orange)

Recursive `Agent` nodes and one primitive, `send`. Every agent answers
`{"type":"reflect"}` with a description of itself, so there is no client library:
the protocol is the API. Given only those readmes, an AI wires the system itself.

Every edge is sealed: all IO (kernel↔kernel, disk, browser) crosses a gated bridge
that is deny-all until opened, and one `reflect` shows what is open.

## Run

```sh
podman run -d -p 127.0.0.1:8088:8088 -v "$PWD":/work \
  ghcr.io/aisixteen/fantastic:arm64      # :amd64 on Intel/AMD; docker works the same
```

The image composes nothing on its own: give an AI the URL and a
[recipe](container/recipes.md), and it builds on the folder mounted at `/work`.
See [`container/README.md`](container/README.md).

<!-- CLI SECTION — written and kept current by the CLI session; the lib side does not edit between these markers. -->

## The `fantastic` CLI

`fantastic` runs the Rust kernel in-process in the current directory (`.fantastic/`),
with a terminal UI, an optional browser canvas, and an MCP face for coding CLIs.

```sh
fantastic                       # the terminal UI
fantastic canvas                # plus the browser canvas
fantastic ai vllm -p PORT       # the root AI on a backend (ollama, responses, anthropic too)
fantastic mcp                   # MCP over stdio
fantastic readme [key]          # the docs
```

<!-- /CLI SECTION -->

## Runtimes

Three interchangeable hosts share one wire protocol and one `.fantastic/` workdir
(one active per workdir), plus a browser frontend:

- [`src/lib/python/`](src/lib/python/): the reference; when runtimes disagree, Python is right
- [`src/lib/rust/`](src/lib/rust/): server and CLI host
- [`src/lib/swift/`](src/lib/swift/): Apple platforms, linked in-process by the
  [Apple client](https://github.com/Alexadar/fantastic_app)
- [`src/lib/ts/`](src/lib/ts/): the browser frontend kernel, served by any host

A change to the protocol, the on-disk format or `reflect` lands in every host.

## License & brand

AGPL-3.0-or-later ([`LICENSE`](LICENSE)). This repo, the
[Apple client](https://github.com/Alexadar/fantastic_app) and the
[relay](https://github.com/Alexadar/fantastic_relay) are one product family by
Koreniuk Oleksandr (aisixteen, kvazis@gmail.com).

**Trademarks (AGPL §7):** **AISIXTEEN®** (U.S. Reg. No. 7,238,635; Int'l Reg. No.
1720602) is a registered trademark of Koreniuk Oleksandr Vasylovych; "Aisixteen
Fantastic" is a trademark of the same owner. The license grants no rights in these
marks, so a fork must ship under a different name.
