# E87 `skins` — the one skin per backdrop and the skin agent

Phase 4 (#49): every colour, radius, font family, shadow and motion value of
the canvas comes from the SKIN (W3C design tokens, compiled in the page), fixed
by the backdrop: `dark` on the web, `glass` in the app's window. The page agent
`skin` is read only (`get`, `schema`). Seeded: one html cell and one terminal
pair (as E65 seeds one).

PLAIN TAB (backdrop web):

1. `<html data-skin="dark" data-backdrop="web">`;
2. dark's computed styles are today's: the body `rgb(6, 6, 12)`, a cell's bezel
   (`::before`) `rgba(15, 15, 25, 0.88)`, `--fc-color-accent-base` is
   `rgb(183 140 255)`, `#bg[data-background]` is `drawn`;
3. `get dark` and `schema` answer; `list`, `use`, `add` and `delete` are unknown
   verbs (`unhandled`); the page still shows dark;
4. on disk, `.fantastic/web/agents/canvas/agents/skin/agent.yaml` holds no
   `skins` and no `user_skins`;
5. the backgrounds panel has the one heading, "Backgrounds": no Skins section,
   no skin rows.

SHELL-EMULATED TAB (backdrop native): `Page.addScriptToEvaluateOnNewDocument`
sets `window.__FANTASTIC_SHELL__ = Object.freeze({name: "probe", os: "macos",
webview: "wkwebview", backdrop: "native"})`, then the page opens again.

6. `data-skin` glass, `data-backdrop` native; the body is `rgba(0, 0, 0, 0)`;
   the bezel's alpha is below 0.6; `#bg[data-background]` is `none` while the
   background's record on disk still holds its `bg_source`; the terminal's
   xterm theme foreground is `#ececec`.
