# kiosk — fantastic OS, a kernel derived from fantastic

The kiosk is the fantastic base plus the machine's agents (`power`, `network`,
`audio`, `displays`, `notifications`, `lock`). It is built ON the base and the base
knows nothing about it. This folder is also the reference for **extending fantastic
from outside**: it uses only what any outside project can use (the base's public
crates, its record format, an agent record's `view`) and changes no base source file.

## How it is put together

1. **A crate that adds bundles.** `fantastic-kiosk` (its own Cargo workspace, outside
   the base's) depends on the base as a git dependency (`Cargo.toml`, never a `path`
   into `../src`): `fantastic-tui` is the whole shell as a library, `fantastic-kernel`
   is what a bundle is written against. The bundles live here:
   `src/fantastic-system-agents` (one bundle per agent, every OS call in
   `sys/linux`) and `src/fantastic-system` (their pure models, no I/O).
2. **The binary** is the base's entry with the kiosk's bundles (`src/main.rs`):
   `fantastic_tui::main_with(args, &[fantastic_kiosk::register])`. `register` is the
   base's `Extra`: it adds the six handler modules to the registry after the stock set,
   so a record naming `power.tools` hydrates into a live agent.
3. **The records.** `ensure` (`src/lib.rs`) makes each agent under `user` with its
   fixed id, `callable_by {user: verbs}` and its **view inline in the record**
   (`view: {kind: html, bytes, calls}`), and re-sets all of it on every boot, so a new
   build's view replaces the old one. No bundle file, no boot code for the views.
4. **The first run** is seeded from a kiosk blueprint with the base's
   `fantastic_tui::seed_dir_with` (#480): `main.rs` calls it with `src/blueprint.yaml` in a `canvas` folder before `main_with`.
5. **Tests.** `cargo test --workspace` in this folder: the pure models, each bundle
   against a fake bus/fixtures (`tests/fake_*.rs`), the boundary (`tests/boundary.rs`:
   the base's dependency closure has none of the six agents' crates, the kiosk's has
   both, and `register` adds exactly the six handler modules), and `ensure`
   (`src/lib.rs`). `cargo test -p fantastic-system-agents -- --ignored` runs the D-Bus
   tests (needs `dbus-daemon` and `python3-dbusmock`; CI: `.github/workflows/kiosk.yml`).
   The pure TypeScript: `node --test "src/canvas/tests/*.test.ts" "src/lock/canvas/tests/*.test.ts"`
   (Node 22+ with type stripping).
   E91 (`e2e/tests/canvas/E91_system_views`) drives the six cells in a real browser against
   the kiosk binary: `FANTASTIC_BIN=$PWD/target/debug/fantastic-kiosk CHROME=<chrome>
   uv run python e2e/tests/canvas/E91_system_views/probe.py` from `src/lib/python`.

## Build

```sh
cd kiosk
cargo build --workspace            # target/debug/fantastic-kiosk
```

The base is taken from its git branch `main`. To build against a local checkout (this
repository's `src/`), copy `.cargo/config.toml.example` to `.cargo/config.toml` (not
committed). There is no committed `Cargo.lock` yet (`.gitignore` holds it back; the boundary test reads the
one a build makes): it is committed once the base without the
system agents is on `main` (a lock made against a patched checkout names no commit).

The image installs the binary as `/usr/bin/fantastic` (the app's sidecar path).

## What moved here from the base

| from the base | to |
|---|---|
| `src/tui/bundles/fantastic-system-agents`, `src/tui/crates/fantastic-system` | `src/fantastic-system-agents`, `src/fantastic-system` |
| `kern/system.rs` (`ensure`, the `system` recipe) | `src/lib.rs` (`ensure`, `register`) |
| `kern/home_origin.rs` | `src/lock/home_origin.rs` (source only: not wired, see below) |
| `home_view/lock_screen.ts`, `lock_model.ts`, the lock overlay CSS, their test | `src/lock/canvas/` |
| `action_popup.ts` `systemTileSize` and its test | `src/canvas/` |
| `os/` (session, mkosi, QEMU checks), `scripts/build/os.sh`, the session half of `scripts/build/deb.sh` | `os/` (`os/build/debs.sh` builds the packages; `os/build/os.sh` the image) |
| `docs/system.md` (`fantastic readme system`) | `docs/system.md` |
| E91 `system_views`, E92 `lock_overlay` | `e2e/tests/` |
| CI job "system agents (fake bus)", the `debs`/`os` jobs of `build.yml` | `.github/workflows/kiosk.yml` (`session-package`, `image`) (checked by `ci/test_kiosk_workflow.py`) |

## Open steps (what the base has to give first)

The base is not changed to make these work; each is a generic gap, to be its own issue.

- **`ensure` on every boot**: `ensure(&Arc<Kernel>)` is ready and tested, but nothing calls
  it. The base's hook, `Extra = fn(&mut BundleRegistry)` (`src/tui/src/lib.rs:68`), sees only
  the registry. What is missing is a call after the host boots with the kernel in hand, at
  the place where `kern::system::ensure` ran: `src/tui/src/vm.rs:237` (after the canvas, before
  `reach::start`; the block was `vm.rs:239-258` at a9b7a316).
- **The lock screen**: the overlay was mounted by the Home page (`home_view.ts`, around
  `const root` / the `show` and `onKey` / `unmount` functions) and the Home's origin was let
  into the system canvas by `home_origin::spawn` (called at `vm.rs:249` of a9b7a316, from the same place: a `Host` and the `web` record).
  The base has no place for an outside page component on the Home page and no post-boot hook.
- **`lock` narrowed to `user`**: `src/tui/src/kern/reach.rs:115` (`NARROWED`, used at `:131` and
  `:168`) no longer names `lock`. Reach has no id-free signal to read: `lock`'s record is
  `callable_by {user: verbs}` exactly like `power`'s, and reach adds every AI holder to the
  latter. Until the base can tell them apart, an AI holder is granted the kiosk's `lock`.
- **The popup's host tiles** (a tile per agent that places its view):
  `action_popup.ts` (the `system` pick and `Presets.system`) and `canvas.ts` (`presets`, `onPick`),
  fed by `backend presets` (`kern/backend.rs`). No generic "host agents offered as tiles".
- **`fantastic readme system`**: `src/tui/src/readme.rs` `CLI_DOCS` is a fixed list; the kiosk's
  doc (`docs/system.md`) has no way into the binary's readme.
- **`os/`** is `kiosk/os/` now (image, session package, QEMU checks, `build/debs.sh`, `build/os.sh`); the app's flag is `--fullscreen` and the base's workflows name no kiosk.
