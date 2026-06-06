# Changelog

All notable changes to `fantastic-canvas`. This project is pre-1.0 (alpha);
the wire protocol and on-disk `.fantastic/` format may change between releases.

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
