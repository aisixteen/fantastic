# The per-OS smoke checklist

A fixed checklist (phase 7, #52), run once per OS against the installers of
one release candidate: the UNSIGNED installers an `installers.yml` rehearsal
builds from the binaries a green CI run tested
(`gh workflow run installers.yml -f ci_run=<full run id>`; see
`src/lib/rust/RELEASING.md`). Signing is the maintainer's, by hand, and is not
part of this checklist.

| OS | installer |
|---|---|
| macOS (Apple silicon) | `fantastic-app-aarch64-apple-darwin.dmg` |
| Windows (x64) | `fantastic-app-x86_64-pc-windows-msvc-setup.exe` |
| Linux (x86_64; Ubuntu 24.04+, Debian 13+) | `fantastic-app-x86_64-unknown-linux-gnu.deb` |

An unsigned build asks to be let through once: on macOS, right-click → Open
(or `xattr -dr com.apple.quarantine`); on Windows, SmartScreen's "More info →
Run anyway". That prompt is the missing signature, not a failure of S1.

## The report

Each run is one comment on #52:

```
smoke: <OS> — <app version>, <installer file name>
OS and version:  …
session:         X11 | Wayland            (Linux only)
GPU and driver:  …
display scale:   …

| id  | result      | note |
|-----|-------------|------|
| S1  | pass / fail | …    |
…
| S12 | pass / fail / n/a | … |
```

Fill in a note for every fail, and for a pass that needed anything unusual.
A re-run after a fix says which fix it checks.

## The items

| id | do | passes when |
|---|---|---|
| S1 | install from the 6b artifact; launch from Dock / Start / the app menu | the Home page shows within 5 s |
| S2 | start and open a fantastic from Home | its canvas shows |
| S3 | look at the backdrop (drag the window over a colourful desktop) | macOS: the native material shows through cells; Windows: Mica or acrylic; Linux: the opaque dark skin with its background; no white flash on open |
| S4 | `fantastic send '{"target":"<page guid>\|view_root","payload":{"type":"terminal_renderer"}}'` | `in_use` is `webgl` on macOS and Windows, `dom` on Linux; `why` names the shell's webview |
| S5 | in one terminal run `while :; do date; sleep 0.05; done` (PowerShell: `while(1){Get-Date; sleep -m 50}`); in another, type `abcdefghij` quickly three times | each character shows before the next is typed; no batching or stalls |
| S6 | run `htop` (or `top`) and `printf '┌─┬─┐\n│a│b│\n└─┴─┘\n'`; zoom the canvas and click inside the terminal | box lines join; no clipped last row or column; the click lands on the clicked cell |
| S7 | look at terminal text | uniform monospace, no proportional fallback face |
| S8 | set the display scale to 200% (Windows), 125–150% fractional (GNOME or KDE), or Retina | text crisp; terminals fit their cells; cell chrome aligned |
| S9 | Home / 1–9 / close shortcuts with ⌘ (macOS) or Ctrl; ⌘K / Ctrl-K; Esc from a full view; Ctrl-C in a terminal | each acts once; Ctrl-C reaches the shell |
| S10 | paste text and an image into a terminal; drag a folder onto Home | the text is typed; the image's path is typed; the folder is registered |
| S11 | quit the app, relaunch | the canvases it started kept running and show as running |
| S12 | Linux only: S1–S6 on X11 and again on Wayland; on an NVIDIA GPU when one is available | no blank window; no `Error 71` crash |

The page guid for S4 is the `guid` in the canvas folder's
`.fantastic/web/agents/canvas/agent.yaml`. Run `fantastic send` in that folder.

## What gets fixed where

Fixed in #52, at most 5 fixes in all: failures of S3–S9 and S12 caused by
either:

- the canvas page (`src/tui/web/canvas`), or
- the app shell's per-OS window or webview configuration: its `sys` modules,
  its Tauri config, and the environment its Linux launcher sets.

Each fix lands with a check: a unit test, a probe step, or the item passing on
a re-run, noted in the report comment. With more failures than the cap, fix
them in the order S12, S4, S5, S6, S3, S7, S8, S9, and file the rest as issues
linked from #52.

Elsewhere:

- installer and signing failures (mostly S1) go to #51;
- shell logic (S2, S10, S11) goes to #48;
- kernel failures get a new issue.

Known candidates. Check these first, and change them only if a run fails them:

- **The terminal font** (S6, S7). The stock skins' `font.family.terminal` is
  `'SF Mono', 'Menlo', monospace`, with no Windows or Linux face before the
  generic family. The fix is to prepend `"Cascadia Mono", "Consolas",
  "DejaVu Sans Mono", "Noto Sans Mono"` before `monospace`, in both stock skins.
- **Linux, Wayland with NVIDIA** (S12). The variables from Tauri's Linux
  graphics guide, set by the app before the webview starts, in its Linux `sys`
  module. Try `__NV_DISABLE_EXPLICIT_SYNC=1` first. Use
  `WEBKIT_DISABLE_DMABUF_RENDERER=1` only for the DMABUF error, since it costs
  speed.
