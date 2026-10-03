# The demo — how Aisixteen Fantastic works

Animated, schematic diagrams in sections:
1. the kernel and its agents;
2. how an AI agent's context works;
3. the terminal UI and the CLI;
4. how the canvas works.

One page for a phone and a desktop. Each diagram has a WIDE and a TALL layout,
and the stage picks one from its own size. Swipe, arrow keys, the buttons and the
step dots all move the same playhead, and every beat is a link
(`#<section>/<scene>/<step>`). Reduced motion is respected.

## Build and run

    sh scripts/pack.sh        # → dist/index.html: one self-contained file
    open dist/index.html      # works from file://; deploy it as a static site
    node --test "tests/**/*.test.ts"

There's no npm and no build step for the tests: the page is bundled with the
library's vendored, sha256-pinned esbuild.

## Add or reorder a section

A section is DATA (`src/model.ts`): scenes of nodes, edges and steps. Each node
has a position on both stages: `wide` is 1000×560 and `tall` is 560×760. A
step names what it reveals (`show`), what it puts in focus, and the messages it
sends along edges (`flow`, with `back` for a reply). Write the section as a
module under `src/sections/` and place it in `src/sections/index.ts`. The tests
check that every id resolves, every node fits both stages, and every flow
travels a visible edge.

## On the canvas

The canvas's action popup has a `demo` kind: it places this page as an html
cell. The canvas pack runs this pack first and embeds `dist/index.html`.

## Later

A GUIDE mode (macOS-Tips-like cards that point at the real UI and advance by
watching the state stream) next to these animations; plan in
`tmp/readme_docs.md`.
