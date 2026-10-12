# Releasing the Rust runtime

The `fantastic` binary's Rust runtime ships as **prebuilt tarballs**
attached to GitHub Releases under the `rust-v*` tag namespace. The
trigger is fully manual — there is no scheduled or auto-tag release.

> **Scope** — this is the Rust runtime only. Python (the reference
> runtime) is development-only today; if a Python release pipeline
> ever ships it'll live in a parallel `python-v*` workflow.

## Targets

Every release builds four targets, named for the `(os, arch)` pair
the consumer cares about — not the Rust triple — so URLs read
naturally and the iOS / Mac apps can pick the right tarball with one
`uname -m` lookup:

| asset | Rust triple | host needs |
|---|---|---|
| `fantastic-macos-aarch64.tar.gz` | `aarch64-apple-darwin` | M-series Macs |
| `fantastic-macos-x86_64.tar.gz` | `x86_64-apple-darwin` | Intel Macs |
| `fantastic-linux-aarch64.tar.gz` | `aarch64-unknown-linux-musl` | ARM Linux (AWS Graviton, RPi server) — self-contained, no glibc dep |
| `fantastic-linux-x86_64.tar.gz` | `x86_64-unknown-linux-musl` | most Linux servers — self-contained, no glibc dep |

Linux targets are **musl-static**, so the SSH-bootstrap binary runs
on any kernel ≥ 3.2 without caring what glibc version the remote host
has. macOS targets are native (no static-link special-casing — Apple
ships their own libc).

Each tarball holds a single `fantastic` binary at the archive root.
Extract directly:

```bash
curl -fsSL <url> | tar -xzC ~/.local/bin
~/.local/bin/fantastic --help
```

## URLs

After a successful release at tag `rust-v0.1.0`:

```
# Version-pinned (recommended for CI / reproducible installs):
https://github.com/Alexadar/fantastic_canvas/releases/download/rust-v0.1.0/fantastic-macos-aarch64.tar.gz

# Floating "latest" (GitHub redirects to whatever's marked latest):
https://github.com/Alexadar/fantastic_canvas/releases/latest/download/fantastic-macos-aarch64.tar.gz
```

Also at the release page: `sha256sums.txt` with checksums for every
artifact.

## How to cut a release

> The release ALWAYS cuts from `main` (or whichever branch you've
> merged the work into). Never from a feature branch.

### 1. Run the pre-flight

```bash
./rust/scripts/release.sh 0.1.0
```

This does, in order:

1. Verifies clean working tree + you're on `main`
2. Bumps `rust/Cargo.toml [workspace.package].version` (and refreshes
   `Cargo.lock` via `cargo check`)
3. Runs `./rust/scripts/quality.sh` (must be **8/8 PASS**)
4. Prints the exact `git tag` + `git push` commands

The script does NOT push or commit on your behalf. Step 4 prints the
two commands; you copy-paste them.

### 2. Commit the bump + push the tag

```bash
git add rust/Cargo.toml rust/Cargo.lock
git commit -m "rust: bump to v0.1.0"
git push                            # the version bump

git tag rust-v0.1.0
git push origin rust-v0.1.0         # fires .github/workflows/release-rust.yml
```

The push of the **tag** is what triggers CI. Pushing the commit alone
does nothing release-wise.

### 3. Wait for CI (~10 min)

`gh run watch` follows the run. When the matrix finishes, the
`release` job creates the GH Release, uploads all four tarballs +
the checksums file, and marks the tag as `latest`.

### 4. Verify

```bash
# Pinned URL works
curl -fIsS https://github.com/Alexadar/fantastic_canvas/releases/download/rust-v0.1.0/fantastic-linux-x86_64.tar.gz \
  | head -1   # → HTTP/2 302  (GH redirects to the CDN)

# Floating-latest URL works
curl -fIsS https://github.com/Alexadar/fantastic_canvas/releases/latest/download/fantastic-macos-aarch64.tar.gz \
  | head -1
```

## Versioning

Standard semver — `MAJOR.MINOR.PATCH`. Pre-releases use SUFFIX dashes
(`0.2.0-rc1`, `1.0.0-beta`). Pre-release tags also become `latest`
in this workflow (intentional — see "`make_latest`" decision below)
unless you manually toggle them off in the GH UI.

| change | bump |
|---|---|
| Wire/protocol break, agent-record schema break | MAJOR |
| New bundle, new verb, new substrate feature | MINOR |
| Bug fix only, no surface change | PATCH |

## Design notes

### Tag namespace — `rust-v*`

Per-runtime release pipelines use prefixed namespaces so they don't
collide: `rust-v*` for the binary release here, `python-v*` reserved
for future Python pipelines. GH Actions tag globs are literal-prefix
matches; the prefixes are disjoint.

### `make_latest: true`

Every successful `rust-v*` tag becomes the GH `latest` redirect. This
keeps the `releases/latest/download/...` URL fresh without operator
clicks. If you cut a pre-release (`rust-v1.0.0-rc1`) that you DON'T
want to publish as latest, edit the release in the GH UI after CI
finishes and uncheck "Set as the latest release".

### Why a separate `release.sh`?

Two reasons:
1. **Local verification** — quality sweep + version bump under your
   own clock, not waiting on CI to discover an obvious problem.
2. **Manual safety gate** — the script prints the tag commands but
   doesn't run them. Operator has to consciously fire CI by pushing
   the tag. No "auto-release on merge" surprises.

## Consuming the release

### SSH bootstrap (Linux + macOS server hosts)

`fantastic-ssh-runner` and `fantastic_app`'s iOS SSH client use this
one-liner to bootstrap a binary on a remote host:

```bash
arch=$(uname -m); os=$(uname -s | tr '[:upper:]' '[:lower:]')
[ "$arch" = "arm64" ] && arch=aarch64    # macOS reports arm64; we use aarch64
curl -fsSL "https://github.com/Alexadar/fantastic_canvas/releases/latest/download/fantastic-${os}-${arch}.tar.gz" \
  | tar -xzC ~/.local/bin
~/.local/bin/fantastic --help
```

The 4 supported `${os}-${arch}` combinations are listed in the
"Targets" table at the top.
