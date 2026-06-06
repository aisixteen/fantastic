<!--
Thanks for the PR. Keep it focused on one concern. Commits/pushes require the
maintainer's consent (project convention) — open from a branch/fork for review.
-->

## What & why

<!-- What does this change, and why? Link any issue. -->

## Runtime(s) touched

- [ ] python
- [ ] rust
- [ ] swift
- [ ] ts (frontend)
- [ ] container
- [ ] docs only

## Cross-runtime parity

- [ ] This change does **not** touch the protocol surface (HTTP/WS/REST shapes,
      system verbs, the `reflect` contract, or the on-disk `.fantastic/` format), **or**
- [ ] It does, and it has landed in **every host** (python is canonical) with the
      parity / bridge tests updated.

## Checks (for each runtime you touched)

- [ ] builds
- [ ] tests pass (`pytest` / `cargo test` / `swift test` / `node --test`)
- [ ] lint + format clean (`ruff` / `cargo fmt` + `clippy` + `deny` / `swift-format` / `tsc`)

## Legal

- [ ] I license this contribution under **AGPL-3.0-or-later** and sign off the
      [DCO](https://developercertificate.org/) (`git commit -s`).
