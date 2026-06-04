# Vendored esbuild — sovereign packer (NO npm)

`pack.sh` bundles the frontend kernel with **this** esbuild binary, never the
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

## Provenance / how it was built (pinned)

Built from source via the Go module proxy (not npm) at a pinned tag:

```sh
GOBIN="$PWD/tools/esbuild/darwin-arm64" \
  go install github.com/evanw/esbuild/cmd/esbuild@v0.25.0
shasum -a 256 tools/esbuild/darwin-arm64/esbuild   # → record in SHA256SUMS
```

- pinned version: **esbuild v0.25.0**
- `darwin-arm64`: sha256 `bf4a6098dc9370bbb4cb5086b4abba2fabc4f25e9be7c715d45a682c3a6de42a`

## Adding a platform

On the target OS/arch, repeat the `go install` with `GOBIN` pointing at
`tools/esbuild/<platform>/` (e.g. `linux-x64`), then append the new line to
`SHA256SUMS`. Keep the same pinned `@vX.Y.Z` so all platforms match one esbuild
release.
