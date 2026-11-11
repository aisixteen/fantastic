# E87 `skins` — the one skin per backdrop and the skin agent

Phase 4 (#49): every colour, radius, font family, shadow and motion value of
the canvas comes from the SKIN (W3C design tokens, compiled in the page), fixed
by the backdrop: `day` on the web, `glass` in the app's window. The page agent
`skin` is read only (`get`, `schema`). Seeded: one html cell and one terminal
pair (as E65 seeds one).

PLAIN TAB (backdrop web):

1. `<html data-skin="day" data-backdrop="web">`;
2. day's computed styles are the day tokens': the body `rgb(237, 237, 237)`, a cell's
   bezel (`::before`) `rgba(255, 255, 255, 0.58)`, `--fc-color-accent-base` is
   `rgb(17 17 17)`, `#bg[data-background]` is `drawn`;
3. `get day` and `schema` answer; `list`, `use`, `add` and `delete` are unknown
   verbs (`unhandled`); the page still shows day;
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
