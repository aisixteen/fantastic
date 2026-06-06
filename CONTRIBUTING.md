# Contributing to Aisixteen Fantastic (`fantastic-canvas`)

Thanks for your interest. This repo is the **open core** of Aisixteen Fantastic —
the host kernels (`python/`, `swift/`, `rust/`) plus the browser frontend
(`ts/`). It is **AGPL-3.0-or-later** and part of one product family with the
Apple client ([`fantastic_app`](https://github.com/Alexadar/fantastic_app)),
same author.

> **Alpha.** Interfaces, the wire protocol, and the on-disk `.fantastic/` format
> still change between releases. Open an issue before a large change.

## Licensing of contributions (DCO)

By contributing you agree your contribution is licensed under
**AGPL-3.0-or-later** (the project license), and you certify the
[Developer Certificate of Origin](https://developercertificate.org/) — i.e. you
have the right to submit it. Sign off each commit:

```sh
git commit -s    # adds "Signed-off-by: Your Name <you@example.com>"
```

We do not require a CLA. The author retains the "Aisixteen Fantastic" /
**AISIXTEEN** trademarks (see `README.md` → *License & brand*); the AGPL covers
the code only, so a **fork must rename**.

## The one rule that matters: cross-runtime parity

There are **three host runtimes** (python, swift, rust) plus the **ts** frontend,
and they share one wire contract. **Python is the canonical reference** — when
implementations disagree, Python is right.

**Any change that affects the protocol surface MUST land in every host:**

- HTTP routes / WS frames / REST shapes,
- system verbs and the `reflect` contract,
- the on-disk `.fantastic/` format.

A protocol change to only one runtime is a bug. The parity is checked
mechanically by `swift/Tests/FantasticParityTests` (spawns the Python kernel and
diffs replies) and the `integration_tests/` bridge matrix. Bundle-local changes
(one bundle's internals, no wire impact) don't need this.

## Build, test, lint — per runtime

Run the relevant runtime's checks before opening a PR. Each runtime's README has
the full detail.

| runtime | build / run | test | lint / format |
|---|---|---|---|
| **python** (`python/`) | `uv sync` · `uv run fantastic` | `uv run pytest -n auto` | `uvx ruff check .` · `uvx ruff format --check .` |
| **rust** (`rust/`) | `cargo build` · `cargo run` | `cargo test` | `cargo fmt --check` · `cargo clippy` · `cargo deny check` |
| **swift** (`swift/`) | `swift build` · `swift run fantastic` | `swift test` | `swift-format` (see `swift/README.md`) |
| **ts** (`ts/`) | `npm run build` · pack: `sh scripts/pack.sh` | `node --test "tests/**/*.test.ts"` | `tsc` (typecheck via `npm run build`) |

Container changes: `sh container/build.sh` then `sh container/test/build_smoke.sh`.

CI runs lint, CodeQL, and spellcheck on every PR; keep them green.

## Commits & pushes

**Commits and pushes to this repo require the maintainer's explicit consent**
(project convention). Open a PR from a fork/branch; the maintainer reviews and
merges. Keep PRs focused — one concern each.

## Reporting

- **Security vulnerabilities:** do **not** open a public issue — see
  [`SECURITY.md`](SECURITY.md) (use GitHub's private vulnerability reporting).
- **Bugs / features:** use the issue templates.
