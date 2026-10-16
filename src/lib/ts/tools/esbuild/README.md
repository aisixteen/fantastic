# Vendored esbuild — sovereign packer (NO npm)

`scripts/pack.sh` bundles an app's page (the entry the app names — the canvas
packs `src/tui/web/canvas/src/bundle.ts`) with **this** esbuild binary, never the
npm `esbuild` wrapper. esbuild is a single dependency-free Go executable; the npm
package merely downloads it. We build it from pinned source instead, so the build
path touches **no npm registry and no node_modules** (active npm supply-chain
threat). The binary is the ONE pinned native dependency.

## Layout

```
tools/esbuild/
  SHA256SUMS              # integrity manifest, verified by pack.sh before every run
  <platform>/esbuild      # the vendored binary, e.g. darwin-arm64/, linux-x64/
```

`pack.sh` selects `<platform>` from `uname -sm` and refuses to run if the binary's
sha256 does not match `SHA256SUMS`.

## Provenance / how it is built (pinned, reproducible)

`pack.sh` builds the binary itself when it is missing (a clean checkout,
`cargo install`): the pinned esbuild release, compiled by the pinned Go toolchain,
with nothing host-specific in it — so every machine gets the same bytes, and the
same sha256 gate verifies it:

```sh
GOTOOLCHAIN=go1.23.4 CGO_ENABLED=0 GOFLAGS=-trimpath \
  GOBIN="$PWD/tools/esbuild/<platform>" \
  go install github.com/evanw/esbuild/cmd/esbuild@v0.25.0
```

Go ≥ 1.21 on PATH is the only requirement (it fetches go1.23.4 itself);
the toolchain and the esbuild module come through Go's checksum database.

- pinned: **esbuild v0.25.0**, built with **go1.23.4**
- platforms in `SHA256SUMS`: `darwin-arm64`, `darwin-x64`, `linux-arm64`,
  `linux-x64`, `win32-x64` (`esbuild.exe`)

## Adding a platform

Build it with the command above (`GOOS`/`GOARCH` set, without `GOBIN`, to build
it from another machine), hash it, add the line to `SHA256SUMS` and its
`uname -sm` case to `pack.sh`. Changing the esbuild or Go version re-hashes every
platform.
