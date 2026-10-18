> Skins — how the canvas looks: W3C design tokens (2025.10) plus a backdrop; the skin agent and the token schema.

# Skins

A SKIN is how the canvas looks: every colour, corner radius, font family,
elevation shadow and motion value its views are drawn with, as DESIGN TOKENS
(the W3C Design Tokens Format Module 2025.10), plus a BACKDROP. A new look is
one token document — no CSS edit, no rebuild, no reload. The page compiles a
skin to one CSS custom property per token (`--fc-<path, . as ->`) in one
`<style id="fantastic-skin">`; switching skins replaces that stylesheet and the
browser restyles everything at once.

A skin is NOT a theme. A THEME (`canvas`, `mobile`, yours) says where the cells
sit — the canvas theme panel's Themes section, `fantastic readme canvas`. A
skin never moves a cell.

## Two backdrops

- `web` — the page paints its own backdrop: `color.page.bg` on the page, and
  the view's background (its `background` agent's GL source) behind every
  cell. A browser tab is always `web`.
- `native` — the page paints nothing behind the cells: the window draws a
  material (macOS vibrancy or Liquid Glass, Windows Mica or acrylic) behind a
  transparent page. The view's background is not drawn — its record is
  untouched, so a web skin brings it back. Glass adds no CSS blur to cells
  (the material blurs; a blurred bezel cost the GPU 27% → 440%).

What hosts the page is said by the shell's flag, `window.__FANTASTIC_SHELL__`
(`fantastic readme canvas`, "Hosted by a shell"): its `backdrop`. No flag — a
plain tab — is `web`.

## The skin agent

Each canvas (peer canvases too) has one page agent `skin` (`skin.ts`), a
delete-locked child of the page root, made when missing. Its record:

- `skins: {web: <name>, native: <name>}` — the skin used on each backdrop.
  The skin IN USE is the slot of this tab's backdrop, so one canvas can show
  glass in the app's window and dark in a browser tab at the same time.
- `user_skins: [{name, base, note, tokens}]` — skins written by an AI or a
  human. `tokens` is a PARTIAL token document merged over `base` (a stock
  skin or another user skin).

Stock skins: `dark` (web — the canvas's own look) and `glass` (native —
translucent surfaces over the window's material).

Verbs (`<page guid>|skin`; an AI cell is granted them when it is spawned or
reopened):

| verb | args | answer / rule |
|---|---|---|
| `list` | — | `{stock: [{name, note, backdrop}], user: [{name, base, note, backdrop, error?}], skins, backdrop: <this tab's>, in_use}` |
| `get` | `name` | `{name, from: stock \| user, base?, note, backdrop, tokens, merged}` — `tokens` as authored, `merged` the full document that compiles |
| `schema` | — | the format, the supported and refused features, and every token: `{path, type, opaque, note, css}` |
| `use` | `name`, `backdrop?: web \| native` | sets that slot (default: this tab's). A skin whose backdrop is `native` is refused for the `web` slot (nothing is behind a browser tab); a web skin may sit in the `native` slot. |
| `add` | `name`, `base`, `tokens`, `note?` | `name` is `^[a-z0-9][a-z0-9_-]{0,39}$`, not a stock name; `base` exists; no base cycle; the merged document must compile, or the refusal names the token path. The same name replaces; in use, the page shows the new version at once. |
| `delete` | `name` | yours only; refused while it is in a slot ("use another first") or another skin's base |

A skin that cannot apply (a record hand-edited into tokens that no longer
compile) is logged and toasted; the page keeps the skin it shows, and `list`
reports it with its `error`.

## Writing a skin

1. Reflect the page and find `skin`.
2. `schema` names the tokens.
3. `get {name: "dark"}` is a complete example (`merged`).
4. `add {name: "ocean", base: "dark", tokens: {color: {accent: {base: {$value: {colorSpace: "srgb", components: [0.2, 0.7, 0.9], hex: "#33b3e6"}}}}}}`
5. `use {name: "ocean"}` — the page shows it, live.

A token is an object with `$value`; a group is an object without one, and a
group's `$type` is inherited by its tokens. `mergeTokens` merges groups deeply
and a token you give REPLACES the base's whole token. The document root's
`$extensions.fantastic.backdrop` (`web` | `native`) is required on a stock
skin; a user skin keeps its base's unless it sets its own.

THE SUPPORTED SUBSET:

- names: lowercase letters and digits, words joined by `-` (they become CSS
  property names); never `{`, `}`, `.` or a leading `$`
- `$type`, `$description`, `$extensions` (unknown keys kept), `$deprecated`
  (accepted, ignored); an object holding both `$value` and children is refused
- aliases: `"$value": "{color.accent.base}"` — a whole token; chains followed;
  an alias takes its target's type; a cycle, a group or a missing path is
  refused
- `color`: `{colorSpace: "srgb", components: [r, g, b] in [0, 1], alpha?: [0, 1] (1), hex?: "#rrggbb"}` — `hex`, when given, must equal the components at 8 bits
- `dimension`: `{value, unit: px | rem}` · `duration`: `{value, unit: ms | s}`
- `cubicBezier`: `[x1, y1, x2, y2]`, x1 and x2 in [0, 1]
- `fontFamily`: a name or an array of names · `fontWeight`: 1–1000 or a
  spec alias (`thin` … `ultra-black`) · `number`
- `shadow`: `{color, offsetX, offsetY, blur, spread, inset?}` or an array of
  them; a member may be an alias of its type

REFUSED, by name: `$ref` (JSON Pointer references), `$extends`, `$root`; the
types `strokeStyle`, `border`, `transition`, `gradient`, `typography`; any
colour space but srgb, and the `none` colour keyword.

`palette` is a free top-level group: any names, alias targets only
(`{palette.sea}`), never compiled to CSS.

OPAQUE tokens (marked below) are mixed at an alpha by the CSS
(`color-mix(in srgb, <token> N%, transparent)`, which reproduces
`rgba(r, g, b, N/100)` exactly): a skin must give them alpha 1.

A skin must give every token below, with its type, and no other (but
`palette.*`).

## The schema

| path | type | CSS name | paints | dark | glass |
|---|---|---|---|---|---|
| `color.page.bg` | color | `--fc-color-page-bg` | html/body, the GL clear colour, the refused box | `rgb(6 6 12)` | `rgb(0 0 0 / 0)` |
| `color.page.text` | color | `--fc-color-page-text` | body text | `rgb(170 170 170)` | `rgb(196 196 204)` |
| `color.ink` (opaque) | color | `--fc-color-ink` | the overlay hue: hairlines, hover fills, glints (always via tint) | `rgb(255 255 255)` | `rgb(255 255 255)` |
| `color.shade` (opaque) | color | `--fc-color-shade` | the shadow hue: wells, veils (via tint) | `rgb(0 0 0)` | `rgb(0 0 0)` |
| `color.text.bright` | color | `--fc-color-text-bright` | headings, active and hovered text | `rgb(255 255 255)` | `rgb(255 255 255)` |
| `color.text.primary` | color | `--fc-color-text-primary` | body text of panels, dialogs, popup | `rgb(236 233 245)` | `rgb(244 242 250)` |
| `color.text.cool` (opaque) | color | `--fc-color-text-cool` | secondary text (always at an alpha) | `rgb(220 230 245)` | `rgb(230 238 250)` |
| `color.text.muted` | color | `--fc-color-text-muted` | leads, labels | `rgb(163 156 186)` | `rgb(188 182 208)` |
| `color.text.subtle` | color | `--fc-color-text-subtle` | step labels, small print | `rgb(143 136 166)` | `rgb(168 162 191)` |
| `color.text.faint` | color | `--fc-color-text-faint` | hints, empty states | `rgb(111 104 134)` | `rgb(140 134 163)` |
| `color.text.code` | color | `--fc-color-text-code` | tool blocks, reflect text | `rgb(170 187 187)` | `rgb(194 208 208)` |
| `color.text.dim` | color | `--fc-color-text-dim` | tool state, others' hints | `rgb(102 102 119)` | `rgb(138 138 154)` |
| `color.accent.base` (opaque) | color | `--fc-color-accent-base` | the accent and all its tints | `rgb(183 140 255)` | `rgb(195 160 255)` |
| `color.accent.text` (opaque) | color | `--fc-color-accent-text` | text on accent tints | `rgb(230 220 255)` | `rgb(239 232 255)` |
| `color.accent.soft` | color | `--fc-color-accent-soft` | thinking text, tip emphasis | `rgb(203 184 240)` | `rgb(216 200 246)` |
| `color.accent.on` | color | `--fc-color-accent-on` | text on a solid accent button | `rgb(22 18 31)` | `rgb(22 18 31)` |
| `color.accent.deep` (opaque) | color | `--fc-color-accent-deep` | user bubble, send, busy pill (tints) | `rgb(110 90 220)` | `rgb(122 102 230)` |
| `color.accent.deep-hover` (opaque) | color | `--fc-color-accent-deep-hover` | send hover (tint) | `rgb(130 110 240)` | `rgb(142 124 244)` |
| `color.status.ok` (opaque) | color | `--fc-color-status-ok` | ok, start button, kept badge | `rgb(126 226 168)` | `rgb(134 236 176)` |
| `color.status.ok-hover` | color | `--fc-color-status-ok-hover` | start button hover | `rgb(165 240 197)` | `rgb(176 244 204)` |
| `color.status.ok-on` | color | `--fc-color-status-ok-on` | text on the start button | `rgb(13 26 18)` | `rgb(13 26 18)` |
| `color.status.ok-deep` (opaque) | color | `--fc-color-status-ok-deep` | streaming pill (tint) | `rgb(80 180 120)` | `rgb(88 192 128)` |
| `color.status.monitor` (opaque) | color | `--fc-color-status-monitor` | monitor delivery (tints) | `rgb(110 200 160)` | `rgb(120 210 170)` |
| `color.status.monitor-text` | color | `--fc-color-status-monitor-text` | monitor delivery text | `rgb(225 240 232 / 0.92)` | `rgb(232 246 238 / 0.95)` |
| `color.status.warn` (opaque) | color | `--fc-color-status-warn` | notices, inquiries, queued | `rgb(255 209 102)` | `rgb(255 214 119)` |
| `color.status.warn-text` | color | `--fc-color-status-warn-text` | inquiry text | `rgb(243 236 214)` | `rgb(248 242 222)` |
| `color.status.warn-muted` | color | `--fc-color-status-warn-muted` | inquiry status | `rgb(217 201 154)` | `rgb(226 212 168)` |
| `color.status.warn-on` | color | `--fc-color-status-warn-on` | text on a warn button | `rgb(30 24 6)` | `rgb(30 24 6)` |
| `color.status.warn-surface` | color | `--fc-color-status-warn-surface` | inquiry button | `rgb(42 37 22)` | `rgb(42 37 22 / 0.7)` |
| `color.status.warn-field` | color | `--fc-color-status-warn-field` | inquiry input | `rgb(27 24 16)` | `rgb(27 24 16 / 0.6)` |
| `color.status.warn-line` | color | `--fc-color-status-warn-line` | inquiry borders | `rgb(90 74 34)` | `rgb(106 88 48)` |
| `color.status.gold` (opaque) | color | `--fc-color-status-gold` | the main canvas ★, main badge | `rgb(255 211 107)` | `rgb(255 217 125)` |
| `color.status.error` (opaque) | color | `--fc-color-status-error` | errors, delete × | `rgb(255 138 138)` | `rgb(255 154 154)` |
| `color.status.error-text` | color | `--fc-color-status-error-text` | toast text | `rgb(255 179 179)` | `rgb(255 192 192)` |
| `color.status.error-deep` (opaque) | color | `--fc-color-status-error-deep` | stop button (tint) | `rgb(200 80 80)` | `rgb(208 90 90)` |
| `color.status.lock` (opaque) | color | `--fc-color-status-lock` | the locked 🔒 chip | `rgb(247 118 142)` | `rgb(255 138 160)` |
| `color.status.info` | color | `--fc-color-status-info` | recall verb | `rgb(154 208 255)` | `rgb(168 216 255)` |
| `color.status.info-muted` | color | `--fc-color-status-info-muted` | tool target | `rgb(136 170 221)` | `rgb(156 186 230)` |
| `color.side.page` (opaque) | color | `--fc-color-side-page` | page-side bezel border (tint) | `rgb(120 170 255)` | `rgb(134 180 255)` |
| `color.side.page-text` | color | `--fc-color-side-page-text` | page agents in the tree | `rgb(140 185 255 / 0.95)` | `rgb(160 198 255)` |
| `color.side.host` (opaque) | color | `--fc-color-side-host` | host-side bezel border (tint) | `rgb(255 190 90)` | `rgb(255 198 108)` |
| `color.side.host-text` | color | `--fc-color-side-host-text` | host agents in the tree | `rgb(255 200 120 / 0.95)` | `rgb(255 210 144)` |
| `color.surface.bezel` | color | `--fc-color-surface-bezel` | a cell's bezel | `rgb(15 15 25 / 0.88)` | `rgb(18 18 30 / 0.42)` |
| `color.surface.bezel-flat` | color | `--fc-color-surface-bezel-flat` | the mobile bezel | `rgb(17 17 26)` | `rgb(17 17 26 / 0.55)` |
| `color.surface.raised` | color | `--fc-color-surface-raised` | the stopped-terminal box | `rgb(17 17 26 / 0.92)` | `rgb(20 20 31 / 0.72)` |
| `color.surface.popup` | color | `--fc-color-surface-popup` | the action popup | `rgb(22 20 32 / 0.96)` | `rgb(24 22 36 / 0.74)` |
| `color.surface.pill` | color | `--fc-color-surface-pill` | the full-view label | `rgb(20 18 30)` | `rgb(20 18 30 / 0.7)` |
| `color.surface.panel` | color | `--fc-color-surface-panel` | wizard, not-set-up card, theme form | `rgb(15 14 22)` | `rgb(15 14 22 / 0.45)` |
| `color.surface.field` | color | `--fc-color-surface-field` | inputs and selects | `rgb(26 24 36)` | `rgb(0 0 0 / 0.25)` |
| `color.surface.button` | color | `--fc-color-surface-button` | buttons | `rgb(36 31 53)` | `rgb(255 255 255 / 0.08)` |
| `color.surface.badge` | color | `--fc-color-surface-badge` | the terminal's last-command bar | `rgb(10 10 18 / 0.72)` | `rgb(10 10 18 / 0.5)` |
| `color.surface.veil` | color | `--fc-color-surface-veil` | the stopped-terminal veil | `rgb(6 6 12 / 0.66)` | `rgb(6 6 12 / 0.45)` |
| `color.surface.toast` | color | `--fc-color-surface-toast` | the error toast | `rgb(40 20 26 / 0.9)` | `rgb(40 20 26 / 0.78)` |
| `color.surface.bubble` | color | `--fc-color-surface-bubble` | an assistant bubble | `rgb(36 38 56 / 0.5)` | `rgb(36 38 56 / 0.35)` |
| `color.surface.code` | color | `--fc-color-surface-code` | a tool block | `rgb(24 26 40 / 0.5)` | `rgb(24 26 40 / 0.35)` |
| `color.surface.well` | color | `--fc-color-surface-well` | a tool pre, the stream view | `rgb(8 10 18 / 0.55)` | `rgb(8 10 18 / 0.38)` |
| `color.line.button` | color | `--fc-color-line-button` | button and control borders | `rgb(49 44 64)` | `rgb(255 255 255 / 0.14)` |
| `color.line.strong` | color | `--fc-color-line-strong` | hovered control borders | `rgb(74 74 77)` | `rgb(255 255 255 / 0.26)` |
| `color.line.faint` | color | `--fc-color-line-faint` | row separators | `rgb(29 29 31)` | `rgb(255 255 255 / 0.07)` |
| `color.terminal.bg` | color | `--fc-color-terminal-bg` | xterm theme.background | `rgb(0 0 0 / 0)` | `rgb(0 0 0 / 0)` |
| `color.terminal.fg` (opaque) | color | `--fc-color-terminal-fg` | xterm theme.foreground, the stopped box text | `rgb(229 229 229)` | `rgb(236 236 236)` |
| `color.terminal.cursor` | color | `--fc-color-terminal-cursor` | xterm theme.cursor | `rgb(229 229 229)` | `rgb(255 255 255)` |
| `shadow.bezel` | shadow | `--fc-shadow-bezel` | a cell's bezel (.agent-frame::before) | `0px 8px 32px 0px rgb(0 0 0 / 0.4), inset 0px 1px 0px 0px rgb(255 255 255 / 0.04)` | `0px 10px 30px 0px rgb(0 0 0 / 0.22), inset 0px 1px 0px 0px rgb(255 255 255 / 0.12)` |
| `shadow.bezel-lifted` | shadow | `--fc-shadow-bezel-lifted` | a dragged or resized cell's bezel | `0px 12px 48px 0px rgb(0 0 0 / 0.6), inset 0px 1px 0px 0px rgb(255 255 255 / 0.08)` | `0px 14px 44px 0px rgb(0 0 0 / 0.32), inset 0px 1px 0px 0px rgb(255 255 255 / 0.16)` |
| `shadow.popup` | shadow | `--fc-shadow-popup` | the action popup | `inset 0px 1px 0px 0px rgb(255 255 255 / 0.1), 0px 0px 0px 1px rgb(255 255 255 / 0.12), 0px 22px 60px -12px rgb(0 0 0 / 0.65)` | `inset 0px 1px 0px 0px rgb(255 255 255 / 0.16), 0px 0px 0px 1px rgb(255 255 255 / 0.18), 0px 22px 60px -12px rgb(0 0 0 / 0.35)` |
| `shadow.bubble` | shadow | `--fc-shadow-bubble` | a chat message bubble | `inset 0px 1px 0px 0px rgb(255 255 255 / 0.1), 0px 0px 0px 1px rgb(255 255 255 / 0.08), 0px 6px 14px -6px rgb(0 0 0 / 0.45)` | `inset 0px 1px 0px 0px rgb(255 255 255 / 0.14), 0px 0px 0px 1px rgb(255 255 255 / 0.12), 0px 6px 14px -6px rgb(0 0 0 / 0.25)` |
| `shadow.pill` | shadow | `--fc-shadow-pill` | the full-view label | `0px 0px 0px 1px rgb(183 140 255 / 0.35), 0px 8px 24px 0px rgb(0 0 0 / 0.5)` | `0px 0px 0px 1px rgb(195 160 255 / 0.4), 0px 8px 24px 0px rgb(0 0 0 / 0.28)` |
| `shadow.flash` | shadow | `--fc-shadow-flash` | a cell flashed from the tree | `0px 0px 0px 2px rgb(183 140 255), 0px 0px 30px 0px rgb(183 140 255 / 0.6)` | `0px 0px 0px 2px rgb(195 160 255), 0px 0px 30px 0px rgb(195 160 255 / 0.6)` |
| `shadow.button` | shadow | `--fc-shadow-button` | the chat's send button | `inset 0px 1px 0px 0px rgb(255 255 255 / 0.12), inset 0px 0px 0px 1px rgb(255 255 255 / 0.1)` | `inset 0px 1px 0px 0px rgb(255 255 255 / 0.18), inset 0px 0px 0px 1px rgb(255 255 255 / 0.16)` |
| `shadow.focus` | shadow | `--fc-shadow-focus` | a focused chat input | `inset 0px 0px 0px 1px rgb(180 220 255 / 0.45), 0px 0px 0px 3px rgb(120 200 255 / 0.1)` | `inset 0px 0px 0px 1px rgb(180 220 255 / 0.45), 0px 0px 0px 3px rgb(120 200 255 / 0.1)` |
| `shadow.toast` | shadow | `--fc-shadow-toast` | the error toast's ring | `0px 0px 0px 1px rgb(255 120 120 / 0.3)` | `0px 0px 0px 1px rgb(255 120 120 / 0.3)` |
| `radius.tag` | dimension | `--fc-radius-tag` | tags and kbd-like labels | `3px` | `4px` |
| `radius.chip` | dimension | `--fc-radius-chip` | head chips | `4px` | `5px` |
| `radius.key` | dimension | `--fc-radius-key` | keycaps | `5px` | `6px` |
| `radius.control` | dimension | `--fc-radius-control` | buttons, inputs, small boxes | `6px` | `8px` |
| `radius.card` | dimension | `--fc-radius-card` | cards, tool blocks | `8px` | `10px` |
| `radius.panel` | dimension | `--fc-radius-panel` | panels, a cell's body (.frame-body) | `10px` | `12px` |
| `radius.bubble` | dimension | `--fc-radius-bubble` | chat bubbles | `12px` | `14px` |
| `radius.handle` | dimension | `--fc-radius-handle` | the resize handles' outer corner | `14px` | `18px` |
| `radius.bezel` | dimension | `--fc-radius-bezel` | a cell's bezel | `16px` | `20px` |
| `radius.pill` | dimension | `--fc-radius-pill` | pills | `999px` | `999px` |
| `font.family.sans` | fontFamily | `--fc-font-family-sans` | every sans-serif text | `system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif` | `system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif` |
| `font.family.mono` | fontFamily | `--fc-font-family-mono` | every monospace text | `ui-monospace, SFMono-Regular, Menlo, monospace` | `ui-monospace, SFMono-Regular, Menlo, monospace` |
| `font.family.terminal` | fontFamily | `--fc-font-family-terminal` | xterm's fontFamily | `"SF Mono", Menlo, monospace` | `"SF Mono", Menlo, monospace` |
| `duration.hover` | duration | `--fc-duration-hover` | hover transitions of head chips | `150ms` | `150ms` |
| `duration.pulse` | duration | `--fc-duration-pulse` | the busy pulse | `1400ms` | `1400ms` |
| `duration.think` | duration | `--fc-duration-think` | the thinking shimmer | `1100ms` | `1100ms` |
| `easing.standard` | cubicBezier | `--fc-easing-standard` | CSS ease | `cubic-bezier(0.25, 0.1, 0.25, 1)` | `cubic-bezier(0.25, 0.1, 0.25, 1)` |
| `easing.in-out` | cubicBezier | `--fc-easing-in-out` | CSS ease-in-out | `cubic-bezier(0.42, 0, 0.58, 1)` | `cubic-bezier(0.42, 0, 0.58, 1)` |
| `blur.veil` | dimension | `--fc-blur-veil` | the stopped-terminal veil's backdrop blur | `2px` | `2px` |

## What stays literal

- Font SIZES, weights, line heights and letter spacing — layout metrics: a
  cell's head is 32px tall, and a terminal's cell metrics and its fit depend on
  them. A skin must never move a cell.
- Layout geometry: insets, paddings, `0` and `50%` radii.
- The geometry of the 1px `inset 0 0 0 1px …` rings; their colour is a token.
- Content: the stock backgrounds' colour sets (the backgrounds' own), the tips
  card and the empty html panel's seed body — an html cell runs in a sandboxed
  frame, which the skin's CSS custom properties do not reach.
- xterm's 16-colour ANSI palette.

A scan test (`tests/skin_scan.test.ts`) keeps colour, font-stack, radius and
duration literals out of the canvas sources.
