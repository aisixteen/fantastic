# Contributing to Aisixteen Fantastic (`fantastic-canvas`)

This repo is the **open core** of Aisixteen Fantastic — the host kernels
(`python/`, `swift/`, `rust/`) plus the browser frontend (`ts/`). It is
**AGPL-3.0-or-later** and part of one product family with the Apple client (in
development), same author.

> **Outside contributions are not accepted.** Pull requests from outside the
> project are closed unmerged. **Bug reports are welcome** as issues, and
> security problems go through private reporting ([`SECURITY.md`](SECURITY.md)).
> The rest of this guide is the development reference: building, testing and the
> cross-runtime rules.

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

CI (`.github/workflows/ci.yml`) builds every runtime and runs its unit tests on
Linux, macOS and Windows (swift: Linux and macOS). For a quick check, `-f units_os=linux`
runs the units on Linux only; a full run always covers all three. A full run adds every release
binary and the model-free e2e (`e2e/run.py --only-free`) on them. Every run is
started by hand, never on a push or a PR; ready branches are batched into one
temporary `ci/batch-<n>` branch and run once. A bug fix gets the units
(`gh workflow run ci.yml --ref <branch>`), a completed phase or a release the
full run (`gh workflow run ci.yml --ref <branch> -f e2e=true`). Lint is not in CI: run the lint commands above for every runtime you touched,
locally, before opening a PR.

## How issues and PRs run

- **Specs live in GitHub issues**, not in markdown files in the repo. The
  roadmap is epic #57; the project board is
  <https://github.com/orgs/aisixteen/projects/1>.
- **The `aiagent` label** marks an issue an AI agent may implement. Only the
  maintainer adds or removes it. An agent implements only `aiagent` issues and
  never changes the label.
- **Open the PR early and watch it.** Open it as soon as the first working
  commit is pushed (a draft is fine). Read every comment on it and act on it:
  review notes and CI results arrive there. Reply on the PR with what you
  changed.
- **CI is started by the maintainer** (see *Build, test, lint* above), for a
  batch of ready branches at once. Nothing runs on a push. Don't start
  workflows yourself.
- **Signing is manual.** All release signing (macOS codesign and notarization,
  Windows signing, Linux GPG) and crates.io publishing happen by hand on the
  maintainer's machine; no signing secrets go into GitHub. `installers.yml`
  builds and verifies unsigned installers only. Steps:
  [`src/lib/rust/RELEASING.md`](src/lib/rust/RELEASING.md).
- **No wider timeouts to hide a race, and no silent fallbacks.** Fix the cause.

## Commits & pushes

**Commits and pushes to this repo require the maintainer's explicit consent**
(project convention). Work lands from the maintainer's branches; outside pull
requests are not accepted. Keep each change focused — one concern each.

## Reporting

- **Security vulnerabilities:** do **not** open a public issue — see
  [`SECURITY.md`](SECURITY.md) (use GitHub's private vulnerability reporting).
- **Bugs / features:** use the issue templates.
