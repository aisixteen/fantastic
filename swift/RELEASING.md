# Releasing the Swift CLI

The `fantastic` CLI ships as a **prebuilt universal macOS tarball**
attached to GitHub Releases under the `swift-v*` tag namespace. The
trigger is fully manual — there is no scheduled or auto-tag release.

> **Scope** — this is the Swift CLI runtime only. The kernel library
> products (`FantasticKernel`, `FantasticKernelEmbedded`,
> `FantasticKernelFull`, …) are not binary-distributable; they ship
> as source via SwiftPM and consumers reference them by Git tag:
>
>     .package(url: "https://github.com/Alexadar/fantastic_canvas",
>              from: "0.1.0")
>
> The Python kernel is development-only today; it has no release
> pipeline.

## Targets

One asset per release. macOS-only — no Linux target.

| asset | architectures | host needs |
|---|---|---|
| `fantastic-macos-universal.tar.gz` | arm64 + x86_64 (lipo'd) | any Mac on macOS 14+ |

The tarball holds the `fantastic` executable + the SwiftPM resource
bundles it depends on (`.bundle` directories for canvas.html,
transport.js, Three.js, xterm, terminal index.html). Layout must be
preserved on extract — `Bundle.module` lookups at runtime resolve
the bundles by their position relative to the executable.

```bash
# Recommended extract:
mkdir -p ~/.local/share/fantastic
curl -fsSL <url> | tar -xzC ~/.local/share/fantastic
ln -sf ~/.local/share/fantastic/fantastic ~/.local/bin/fantastic
```

## URLs

After a successful release at tag `swift-v0.1.0`:

```
# Version-pinned (recommended):
https://github.com/Alexadar/fantastic_canvas/releases/download/swift-v0.1.0/fantastic-macos-universal.tar.gz

# Floating "latest":
https://github.com/Alexadar/fantastic_canvas/releases/latest/download/fantastic-macos-universal.tar.gz
```

Also at the release page: `sha256sums.txt` with the tarball's
checksum.

## How to cut a release

> The release ALWAYS cuts from `main` (or whichever branch has the
> merged work). Never from a feature branch.

### 1. Run the pre-flight

```bash
swift/scripts/release.sh 0.1.0
```

This does, in order:

1. Verifies clean working tree (warns if not on `main`)
2. Runs `swift build -c release` + `swift test` as quality gate
3. Builds the universal binary (`--arch arm64 --arch x86_64`)
4. Stages the binary + resource bundles into `dist/stage/`
5. Tars into `dist/fantastic-macos-universal.tar.gz`
6. Writes `dist/sha256sums.txt`
7. Prints the exact `git tag` + `gh release create` commands

The script does NOT push, tag, or release on your behalf. Step 7
prints the two commands; you copy-paste them.

### 2. Tag + cut the GitHub release

```bash
git tag swift-v0.1.0
git push origin swift-v0.1.0

gh release create swift-v0.1.0 \
    --title "fantastic CLI 0.1.0" \
    --notes "macOS universal (arm64 + x86_64). See swift/RELEASING.md." \
    dist/fantastic-macos-universal.tar.gz \
    dist/sha256sums.txt
```

## Signing

Today the script ships an **unsigned / ad-hoc** binary. First-run
behavior on a different Mac:

```
"fantastic" cannot be opened because the developer cannot be verified.
```

Workarounds for users:
- `xattr -d com.apple.quarantine ~/.local/bin/fantastic` (one-time)
- right-click → Open (then dismiss the warning)

### Path to fully signed + notarized

When a Developer ID Application certificate is available, fill in the
`sign_and_notarize` hook in `swift/scripts/release.sh`. The block has
the exact sequence in a TODO comment:

```bash
codesign --force --options runtime --timestamp \
    --sign "Developer ID Application: <Team> (<TeamID>)" \
    fantastic

ditto -c -k --keepParent fantastic fantastic.zip
xcrun notarytool submit fantastic.zip \
    --apple-id $APPLE_ID --team-id $TEAM_ID \
    --password $APP_PASSWORD --wait
xcrun stapler staple fantastic
```

After stapling, the tarball user gets zero Gatekeeper friction.

Required env vars (suggest storing in a `.envrc`, NOT committed):
- `APPLE_ID` — Apple ID email
- `TEAM_ID` — Developer Team ID
- `APP_PASSWORD` — app-specific password generated at appleid.apple.com

## Versioning convention

`swift-v<major>.<minor>.<patch>[-<prerelease>]`:
- `swift-v0.1.0` — first stable
- `swift-v0.2.0-rc1` — release candidate
- `swift-v1.0.0` — first non-experimental

Distinct from any historical `rust-v*` tags — the namespaces don't
overlap.

## Rollback

GitHub releases are not auto-promoted; an aborted release just
leaves a draft. If a published release needs to come back:

```bash
gh release delete swift-v0.1.0 --yes
git push --delete origin swift-v0.1.0
git tag -d swift-v0.1.0
```

Re-cut with the next patch version rather than reusing the tag.
