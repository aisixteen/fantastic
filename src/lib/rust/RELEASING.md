# Releasing

A release is the `fantastic` CLI: the Rust kernel in-process, the terminal UI, the
browser canvas and the MCP face. It ships two ways: the crates.io package
`fantastic`, and prebuilt binaries on a GitHub Release. The tag is pushed from a
laptop, and the release workflow is then started by hand on it; GitHub Actions
builds every target, runs the model-free e2e on those binaries and publishes the
GitHub Release (`.github/workflows/release.yml`).
crates.io stays a local step.

## Version

- Releases are tagged `vMAJOR.MINOR.PATCH` (for example `v0.0.4`). The tag is the
  version: `scripts/build/crate.sh` reads the latest tag, or `FANTASTIC_VERSION`
  if it is set. The workspace `Cargo.toml` versions are not bumped.
- The default bump is PATCH; bump MINOR only when it is asked for. While the
  version is pre-1.0, the wire protocol and the `.fantastic/` format may change in
  any release.

## Steps

All steps run from the repo root, on `main`, with a clean tree.

1. **Gate.** Run the quality sweep: ruff check and format for Python; `cargo fmt
   --check` and `cargo clippy --workspace --all-targets -- -D warnings` for Rust.
   The runtime suites must be green.
2. **Changelog.** In `CHANGELOG.md`, add a `## [X.Y.Z] - YYYY-MM-DD` heading under
   `## [Unreleased]`, so the entries collected so far fall under the new version
   and `[Unreleased]` starts empty again. Commit it as `release: X.Y.Z`.
3. **Tag, push, run the release.**
   ```sh
   git tag -a vX.Y.Z -m vX.Y.Z
   git push origin main vX.Y.Z
   gh workflow run release.yml --ref vX.Y.Z
   ```
   The push starts nothing; `gh workflow run` starts
   `.github/workflows/release.yml` on the tag: it refuses a ref that is not a tag on
   `main` or has no `## [X.Y.Z]` CHANGELOG section, builds the archives below
   (`scripts/build/cli.sh`), runs the model-free e2e on them, and publishes the
   GitHub Release with `SHA256SUMS` and that CHANGELOG section as the notes.
4. **crates.io** (local — the registry token never goes to GitHub).
   ```sh
   CARGO_REGISTRY_TOKEN=… sh scripts/build/crate.sh --publish
   ```
   The script generates `dist/crate/fantastic`: one package that holds the CLI and
   every crate it links, as modules, with the canvas page embedded prebuilt, so
   installing it needs no Go. It then runs `cargo package` (a full build) and
   `cargo publish`. Without `--publish` it stops after packaging. Users install
   with `cargo install fantastic --locked`.
5. **GitHub Release** — published by the workflow (step 3): five archives plus
   `SHA256SUMS`:

   | archive | for |
   |---|---|
   | `fantastic-aarch64-apple-darwin.tar.gz` | macOS, Apple silicon (macOS 14+) |
   | `fantastic-x86_64-apple-darwin.tar.gz` | macOS, Intel (macOS 14+) |
   | `fantastic-aarch64-unknown-linux-musl.tar.gz` | Linux arm64, static |
   | `fantastic-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64, static |
   | `fantastic-x86_64-pc-windows-msvc.zip` | Windows x86_64 |

   One archive by hand: `sh scripts/build/cli.sh --target <triple>` writes
   `dist/fantastic-<triple>.tar.gz` (`.zip` for Windows).

   The Linux builds are musl-static, so they run without a matching glibc. The AI
   backends built in are vLLM, Ollama, the OpenAI Responses API and Anthropic.

## Notes

- A tag that is pushed and published to crates.io is permanent: crates.io allows
  only a yank, never a re-upload of the same version. To fix a release, cut the
  next patch.
- The registry token is read from the environment and never written into the
  repo or printed.
