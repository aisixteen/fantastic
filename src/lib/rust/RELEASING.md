# Releasing

A release is the `fantastic` CLI: the Rust kernel in-process, the terminal UI, the
browser canvas and the MCP face. It ships two ways: the crates.io package
`fantastic`, and prebuilt binaries on a GitHub Release. The tag is pushed from a
laptop, and the release workflow is then started by hand on it. It builds
nothing: it publishes the archives a green CI run (`.github/workflows/ci.yml`)
built for that very commit — every target, the model-free e2e passed on those
binaries — as the GitHub Release (`.github/workflows/release.yml`).
crates.io stays a local step. The desktop app's installers are built and
verified by CI (`.github/workflows/installers.yml`) and signed by hand on the
laptop: no signing key is on GitHub (steps 6–9).

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
3. **Tag, push, CI, release.**
   ```sh
   git tag -a vX.Y.Z -m vX.Y.Z
   git push origin main vX.Y.Z
   gh workflow run ci.yml --ref vX.Y.Z -f e2e=true   # skip if a full run is already green on this commit
   gh workflow run release.yml --ref vX.Y.Z          # once it is
   ```
   The push starts nothing. A full CI run (`-f e2e=true`, titled `ci · full`) builds the archives below (`scripts/build/cli.sh`)
   for every target, runs the model-free e2e on them and keeps them for 30 days.
   `.github/workflows/release.yml` refuses a ref that is not a tag on `main`, has
   no `## [X.Y.Z]` CHANGELOG section, or has no green CI run on its commit; it
   publishes that run's archives as the GitHub Release, with `SHA256SUMS` and the
   CHANGELOG section as the notes.
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

   The notes end with `CI run: <url>`, the run whose archives were published. The
   installers (step 6) are built from that run.
6. **Installers: built and verified by CI, unsigned.**
   ```sh
   gh workflow run installers.yml --ref vX.Y.Z
   ```
   `.github/workflows/installers.yml` takes the app (`app-<triple>`) and the CLI
   (`release-<target>`, checked against the Release's `SHA256SUMS`) of the CI
   run the Release names (`CI run:` in its notes), bundles them with
   `--no-sign --no-binary-patching`, installs and launches each installer on a
   fresh runner, and keeps for 7 days, per OS:

   | artifact | holds |
   |---|---|
   | `installer-aarch64-apple-darwin` | the unsigned DMG it verified |
   | `to-sign-aarch64-apple-darwin` | `fantastic-app-aarch64-apple-darwin.app.tar`: the unsigned `Aisixteen Fantastic.app` |
   | `installer-x86_64-pc-windows-msvc` | the unsigned setup it verified |
   | `to-sign-x86_64-pc-windows-msvc` | `fantastic-app-x86_64-pc-windows-msvc.stage.tar`: `fantastic-app.exe` and the sidecar, at the paths the bundler takes them |
   | `installer-x86_64-unknown-linux-gnu` | the two `.deb`s, built by `scripts/build/deb.sh` — they ARE the release packages: the app's, and `fantastic-session_<version>_all.deb` (the kiosk, #54) |

   Its guard refuses, listing every problem at once, a tag not on `main`, a
   Release that is missing or names no CI run, a failed or mismatched run, and
   expired artifacts. On any other ref it builds from `-f ci_run=<id>` (a full
   run whose `binaries / build · …` and `binaries / app · …` jobs succeeded) or
   the newest green `ci · full` run on the commit, and publishes nothing.
7. **Sign by hand, on the laptop.** No signing secret, certificate or key is on
   GitHub. Every command below is run by hand; `RUN` is the installers run id
   of step 6, `V` the version.
   ```sh
   RUN=<installers run id>; V=X.Y.Z
   mkdir -p sign out
   ```
   **macOS** (Developer ID Application certificate in the login keychain; a
   notarytool profile made once with
   `xcrun notarytool store-credentials fantastic --key AuthKey_<id>.p8 --key-id <id> --issuer <issuer>`).
   The app is signed inside out and notarized before the DMG is made; then the
   DMG itself is signed, notarized and stapled:
   ```sh
   ID="Developer ID Application: <Name> (<TEAMID>)"
   gh run download "$RUN" -n to-sign-aarch64-apple-darwin -D sign/mac
   tar -xf sign/mac/fantastic-app-aarch64-apple-darwin.app.tar -C sign/mac
   APP="sign/mac/Aisixteen Fantastic.app"
   codesign --force --timestamp --options runtime -s "$ID" "$APP/Contents/MacOS/fantastic"
   codesign --force --timestamp --options runtime -s "$ID" "$APP"
   codesign --verify --deep --strict --verbose=2 "$APP"
   ditto -c -k --keepParent "$APP" sign/mac/app.zip
   xcrun notarytool submit sign/mac/app.zip --keychain-profile fantastic --wait
   xcrun stapler staple "$APP"
   mkdir sign/mac/dmg && ditto "$APP" "sign/mac/dmg/Aisixteen Fantastic.app" && ln -s /Applications sign/mac/dmg/Applications
   DMG=out/fantastic-app-aarch64-apple-darwin.dmg
   hdiutil create -volname "Aisixteen Fantastic" -srcfolder sign/mac/dmg -fs HFS+ -format UDZO -ov "$DMG"
   codesign --timestamp -s "$ID" "$DMG"
   xcrun notarytool submit "$DMG" --keychain-profile fantastic --wait
   xcrun stapler staple "$DMG"
   spctl -a -vvv -t open --context context:primary-signature "$DMG"   # source=Notarized Developer ID
   ```
   No entitlements: the app is not sandboxed and loads no code at run time.

   **Windows, from the Mac** (Azure Artifact Signing; `brew install nsis jsign azure-cli`;
   `az login` as an identity with the *Artifact Signing Certificate Profile
   Signer* role). The bundler signs `fantastic-app.exe` and the sidecar, packs
   them into the NSIS setup, signs its plugins and uninstaller, then signs the
   setup — every signature through `signCommand`, which runs on macOS and
   Linux too (see the note below):
   ```sh
   gh run download "$RUN" -n to-sign-x86_64-pc-windows-msvc -D sign/win
   tar -xf sign/win/fantastic-app-x86_64-pc-windows-msvc.stage.tar   # at the repo root, on the tag
   rustup target add x86_64-pc-windows-msvc
   npm ci --prefix src/app
   TOKEN=$(az account get-access-token --resource https://codesigning.azure.net --query accessToken -o tsv)
   cat > sign/win/sign.json <<EOF
   {"version":"$V","bundle":{"windows":{"signCommand":{"cmd":"jsign","args":[
     "--storetype","TRUSTEDSIGNING","--keystore","<region>.codesigning.azure.net",
     "--storepass","$TOKEN","--alias","<account>/<certificate profile>",
     "--tsaurl","http://timestamp.acs.microsoft.com","--tsmode","RFC3161","%1"]}}}}
   EOF
   (cd src/app && npx tauri bundle --ci --bundles nsis --target x86_64-pc-windows-msvc \
     --no-binary-patching --config ../../sign/win/sign.json)
   rm sign/win/sign.json
   cp "src/app/target/x86_64-pc-windows-msvc/release/bundle/nsis/Aisixteen Fantastic_${V}_x64-setup.exe" \
     out/fantastic-app-x86_64-pc-windows-msvc-setup.exe
   ```
   *Why this works from macOS* (tauri-bundler 2.10.1, the bundler of tauri-cli
   2.12.1): NSIS bundling is not limited to Windows (`bundle.rs`: "NSIS
   installers can be built in linux+macOS"); off Windows it runs `makensis`
   from `PATH` (`nsis/mod.rs`), and `try_sign` runs the custom `signCommand` on
   any host (`windows/sign.rs`). Checked on Linux, the same non-Windows code
   path, with the CI's `fantastic-app.exe` and sidecar and a stand-in sign
   command: the bundler called it, in order, for the sidecar,
   `fantastic-app.exe`, the five NSIS plugins, the uninstaller and the setup,
   and wrote the setup. Not yet run on a Mac with jsign and a real Azure
   account; the first release does that, and `installers.yml -f check` (step 9)
   confirms every signature on Windows.

   **Linux** (the GPG key whose fingerprint `SECURITY.md` lists). The CI's
   `.deb` is the release package:
   ```sh
   FPR=<40 hex>
   gh run download "$RUN" -n installer-x86_64-unknown-linux-gnu -D out
   for deb in out/fantastic-app-x86_64-unknown-linux-gnu.deb out/fantastic-session_${V}_all.deb; do
     gpg --local-user "$FPR" --armor --detach-sign "$deb"   # → .deb.asc
   done
   gpg --armor --export "$FPR" > out/fantastic-app-linux-signing-key.asc
   ```
   Linux ships as a `.deb` only. There is no AppImage: the distributions'
   WebKitGTK starts its helper processes from a path compiled into the library
   (`WEBKIT_EXEC_PATH` works only in developer builds), so an AppImage works
   only by patching that library, as Tauri's linuxdeploy route does (evidence
   in PR #81). The cross-distribution package is a Flatpak (#82).
8. **Upload** — never `--clobber`: an existing name fails, so a published
   installer is never silently replaced.
   ```sh
   (cd out && shasum -a 256 fantastic-* > SHA256SUMS-app && cat SHA256SUMS-app)
   gh release upload "v$V" out/fantastic-* out/SHA256SUMS-app
   ```

   | asset | for |
   |---|---|
   | `fantastic-app-aarch64-apple-darwin.dmg` | macOS 14+, Apple silicon: Developer ID, the app and the DMG notarized and stapled |
   | `fantastic-app-x86_64-pc-windows-msvc-setup.exe` | Windows 10/11 x64: NSIS, per user, Authenticode-signed and timestamped |
   | `fantastic-app-x86_64-unknown-linux-gnu.deb` (+ `.asc`) | x86_64 Linux: Ubuntu 24.04+, Debian 13+ (the kiosk, #54) |
   | `fantastic-session_<version>_all.deb` (+ `.asc`) | turns a headless Debian 13 (amd64) into the kiosk (#54): `sudo apt install ./fantastic-app-x86_64-unknown-linux-gnu.deb ./fantastic-session_<version>_all.deb`, then reboot. It depends on `aisixteen-fantastic (= <version>)` |
   | `fantastic-app-linux-signing-key.asc` | the public key of the `.asc` signatures |
   | `SHA256SUMS-app` | the sums of all of the above |
9. **Check the Release** (on `main`, so it reads `SHA256SUMS-app` against
   `SECURITY.md`'s current fingerprint):
   ```sh
   gh workflow run installers.yml -f check=vX.Y.Z
   ```
   It builds nothing and never writes to the Release. On fresh runners it
   downloads the installers, checks `SHA256SUMS-app`, the notarization, staple
   and Developer ID of the DMG and the app, the Authenticode signatures (with
   timestamps) of the setup and the installed binaries, and the GPG
   signatures of both `.deb`s against `SECURITY.md`'s fingerprint, then
   installs and launches each, as a build does.
10. **Publish the server image** (Phase 12, #158), after the release, from the
    same commit. It runs only on a commit of a green FULL `ci.yml` run (its
    `container` job smoke-tested that image under Docker and Podman); the
    workflow refuses any other commit. Dispatch it by hand, never from a tag:
    ```sh
    gh workflow run publish-container.yml -f commit=<sha> -f version=X.Y.Z -f latest=true
    ```
    It stages the `release-*` archives of that run (kept 30 days, so publish
    within them), builds `os/container/Containerfile` for `linux/amd64` and
    `linux/arm64` with `scripts/build/container.sh --arch all`, and pushes
    `ghcr.io/aisixteen/fantastic` with `X.Y.Z`, the sha, and `latest` (pass
    `latest=false`, or leave it out, for an older release). It uses GITHUB_TOKEN
    (`packages: write`), not a stored secret. The first push creates the package:
    make it public once, on GitHub.

## Notes

- A tag that is pushed and published to crates.io is permanent: crates.io allows
  only a yank, never a re-upload of the same version. To fix a release, cut the
  next patch.
- The registry token is read from the environment and never written into the
  repo or printed.
