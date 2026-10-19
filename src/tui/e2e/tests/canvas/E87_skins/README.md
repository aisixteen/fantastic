# E87 `skins` — design tokens and the skin agent

Phase 4 (#49): every colour, radius, font family, shadow and motion value of
the canvas comes from a SKIN (W3C design tokens, compiled in the page), picked
per backdrop by the page agent `skin`. Seeded: one html cell and one terminal
pair (as E65 seeds one).

PLAIN TAB (backdrop web):

1. `<html data-skin="dark" data-backdrop="web">`;
2. dark's computed styles are today's: the body `rgb(6, 6, 12)`, a cell's bezel
   (`::before`) `rgba(15, 15, 25, 0.88)`, `--fc-color-accent-base` is
   `rgb(183 140 255)`, `#bg[data-background]` is `drawn`;
3. `window.__skinProbe = 1`; `add {name: probe-red, base: dark}` with a red
   accent and a dark-red bezel → `added`;
4. `add` with a bad colour (`components: [2, 0, 0]`) is refused: `bad_arg`,
   naming `color.accent.base`;
5. `use probe-red` → within 3 s, with no reload (`__skinProbe` still 1), the
   bezel is `rgba(51, 0, 0, 0.9)` and the accent `rgb(255 51 51)`;
6. `use glass` is refused (a native skin in a browser tab) and nothing changes;
7. `use dark` → dark's values are back;
8. on disk, `.fantastic/web/agents/canvas/agents/skin/agent.yaml` holds
   `skins.web: dark` and a `user_skins` entry `probe-red`;
9. the canvas theme panel shows a third heading, "Skins", listing dark, glass
   and probe-red; a click on probe-red applies it.

SHELL-EMULATED TAB (backdrop native): `Page.addScriptToEvaluateOnNewDocument`
sets `window.__FANTASTIC_SHELL__ = Object.freeze({name: "probe", os: "macos",
webview: "wkwebview", backdrop: "native"})`, then the page opens again.

10. `data-skin` glass, `data-backdrop` native; the body is `rgba(0, 0, 0, 0)`;
    the bezel's alpha is below 0.6; `#bg[data-background]` is `none` while the
    background's record on disk still holds its `bg_source`; the terminal's
    xterm theme foreground is `#ececec`;
11. `use {name: dark, backdrop: native}` → live: the body `rgb(6, 6, 12)`, the
    background `drawn`, the terminal foreground `#e5e5e5`; `use {name: glass,
    backdrop: native}` → the transparent page again; no reload;
12. `skins.web` on disk is still `dark`.
