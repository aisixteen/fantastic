// THE SKIN SCHEMA — the design tokens the canvas reads (phase 4, #49): every
// colour, radius, font family, elevation shadow and motion value its views are
// drawn with. A skin (`stock/*.tokens.json`, or one an AI adds through the
// `skin` agent) gives each of them a value; `tokens.ts` compiles it to one CSS
// custom property per token, `--fc-<path with . as ->`.
//
// `opaque` marks a token the CSS mixes with `tint()` (`css.ts`): a skin must give
// it alpha 1, or the tint's alpha would multiply with its own.
//
// What stays literal (font sizes, weights, layout geometry) is said in
// `src/tui/docs/skins.md`: a skin must never move a cell.

export type TokenType = "color" | "dimension" | "duration" | "cubicBezier" | "fontFamily" | "fontWeight" | "number" | "shadow";

export interface SchemaToken {
  readonly path: string;
  readonly type: TokenType;
  readonly opaque?: true;
  /** what it paints */
  readonly note: string;
}

/** A token's CSS custom property: `color.surface.bezel-flat` → `--fc-color-surface-bezel-flat`. */
export const cssName = (path: string): string => `--fc-${path.replace(/\./g, "-")}`;

export const SKIN_SCHEMA: ReadonlyArray<SchemaToken> = [
  { path: "color.page.bg", type: "color", note: "html/body, the GL clear colour, the refused box" },
  { path: "color.page.text", type: "color", note: "body text" },
  { path: "color.ink", type: "color", opaque: true, note: "the overlay hue: hairlines, hover fills, glints (always via tint)" },
  { path: "color.shade", type: "color", opaque: true, note: "the shadow hue: wells, veils (via tint)" },
  { path: "color.text.bright", type: "color", note: "headings, active and hovered text" },
  { path: "color.text.primary", type: "color", note: "body text of panels, dialogs, popup" },
  { path: "color.text.cool", type: "color", opaque: true, note: "secondary text (always at an alpha)" },
  { path: "color.text.muted", type: "color", note: "leads, labels" },
  { path: "color.text.subtle", type: "color", note: "step labels, small print" },
  { path: "color.text.faint", type: "color", note: "hints, empty states" },
  { path: "color.text.code", type: "color", note: "tool blocks, reflect text" },
  { path: "color.text.dim", type: "color", note: "tool state, others' hints" },
  { path: "color.accent.base", type: "color", opaque: true, note: "the accent and all its tints" },
  { path: "color.accent.text", type: "color", opaque: true, note: "text on accent tints" },
  { path: "color.accent.soft", type: "color", note: "thinking text, tip emphasis" },
  { path: "color.accent.on", type: "color", note: "text on a solid accent button" },
  { path: "color.accent.deep", type: "color", opaque: true, note: "user bubble, send, busy pill (tints)" },
  { path: "color.accent.deep-hover", type: "color", opaque: true, note: "send hover (tint)" },
  { path: "color.status.ok", type: "color", opaque: true, note: "ok, start button, kept badge" },
  { path: "color.status.ok-hover", type: "color", note: "start button hover" },
  { path: "color.status.ok-on", type: "color", note: "text on the start button" },
  { path: "color.status.ok-deep", type: "color", opaque: true, note: "streaming pill (tint)" },
  { path: "color.status.monitor", type: "color", opaque: true, note: "monitor delivery (tints)" },
  { path: "color.status.monitor-text", type: "color", note: "monitor delivery text" },
  { path: "color.status.warn", type: "color", opaque: true, note: "notices, inquiries, queued" },
  { path: "color.status.warn-text", type: "color", note: "inquiry text" },
  { path: "color.status.warn-muted", type: "color", note: "inquiry status" },
  { path: "color.status.warn-on", type: "color", note: "text on a warn button" },
  { path: "color.status.warn-surface", type: "color", note: "inquiry button" },
  { path: "color.status.warn-field", type: "color", note: "inquiry input" },
  { path: "color.status.warn-line", type: "color", note: "inquiry borders" },
  { path: "color.status.gold", type: "color", opaque: true, note: "the main canvas ★, main badge" },
  { path: "color.status.error", type: "color", opaque: true, note: "errors, delete ×" },
  { path: "color.status.error-text", type: "color", note: "toast text" },
  { path: "color.status.error-deep", type: "color", opaque: true, note: "stop button (tint)" },
  { path: "color.status.lock", type: "color", opaque: true, note: "the locked 🔒 chip" },
  { path: "color.status.info", type: "color", note: "recall verb" },
  { path: "color.status.info-muted", type: "color", note: "tool target" },
  { path: "color.side.page", type: "color", opaque: true, note: "page-side bezel border (tint)" },
  { path: "color.side.page-text", type: "color", note: "page agents in the tree" },
  { path: "color.side.host", type: "color", opaque: true, note: "host-side bezel border (tint)" },
  { path: "color.side.host-text", type: "color", note: "host agents in the tree" },
  { path: "color.syntax.keyword", type: "color", note: "keywords in the previewer and editor" },
  { path: "color.syntax.string", type: "color", note: "strings in the previewer and editor" },
  { path: "color.syntax.comment", type: "color", note: "comments in the previewer and editor" },
  { path: "color.syntax.number", type: "color", note: "numbers in the previewer and editor" },
  { path: "color.syntax.type", type: "color", note: "types and keys in the previewer and editor" },
  { path: "color.syntax.punct", type: "color", note: "punctuation in the previewer and editor" },
  { path: "color.tree.fantastic", type: "color", opaque: true, note: "the .fantastic folder in the file tree (via tint)" },
  { path: "color.surface.bezel", type: "color", note: "a cell's bezel" },
  { path: "color.surface.bezel-flat", type: "color", note: "the mobile bezel" },
  { path: "color.surface.raised", type: "color", note: "the stopped-terminal box" },
  { path: "color.surface.popup", type: "color", note: "the action popup" },
  { path: "color.surface.pill", type: "color", note: "the full-view label" },
  { path: "color.surface.panel", type: "color", note: "wizard, not-set-up card, theme form" },
  { path: "color.surface.field", type: "color", note: "inputs and selects" },
  { path: "color.surface.button", type: "color", note: "buttons" },
  { path: "color.surface.badge", type: "color", note: "the terminal's last-command bar" },
  { path: "color.surface.veil", type: "color", note: "the stopped-terminal veil" },
  { path: "color.surface.toast", type: "color", note: "the error toast" },
  { path: "color.surface.bubble", type: "color", note: "an assistant bubble" },
  { path: "color.surface.code", type: "color", note: "a tool block" },
  { path: "color.surface.well", type: "color", note: "a tool pre, the stream view" },
  { path: "color.line.button", type: "color", note: "button and control borders" },
  { path: "color.line.strong", type: "color", note: "hovered control borders" },
  { path: "color.line.faint", type: "color", note: "row separators" },
  { path: "color.terminal.bg", type: "color", note: "xterm theme.background" },
  { path: "color.terminal.fg", type: "color", opaque: true, note: "xterm theme.foreground, the stopped box text" },
  { path: "color.terminal.cursor", type: "color", note: "xterm theme.cursor" },
  { path: "shadow.bezel", type: "shadow", note: "a cell's bezel (.agent-frame::before)" },
  { path: "shadow.bezel-lifted", type: "shadow", note: "a dragged or resized cell's bezel" },
  { path: "shadow.popup", type: "shadow", note: "the action popup" },
  { path: "shadow.bubble", type: "shadow", note: "a chat message bubble" },
  { path: "shadow.pill", type: "shadow", note: "the full-view label" },
  { path: "shadow.flash", type: "shadow", note: "a cell flashed from the tree" },
  { path: "shadow.button", type: "shadow", note: "the chat's send button" },
  { path: "shadow.focus", type: "shadow", note: "a focused chat input" },
  { path: "shadow.toast", type: "shadow", note: "the error toast's ring" },
  { path: "radius.tag", type: "dimension", note: "tags and kbd-like labels" },
  { path: "radius.chip", type: "dimension", note: "head chips" },
  { path: "radius.key", type: "dimension", note: "keycaps" },
  { path: "radius.control", type: "dimension", note: "buttons, inputs, small boxes" },
  { path: "radius.card", type: "dimension", note: "cards, tool blocks" },
  { path: "radius.panel", type: "dimension", note: "panels, a cell's body (.frame-body)" },
  { path: "radius.bubble", type: "dimension", note: "chat bubbles" },
  { path: "radius.handle", type: "dimension", note: "the resize handles' outer corner" },
  { path: "radius.bezel", type: "dimension", note: "a cell's bezel" },
  { path: "radius.pill", type: "dimension", note: "pills" },
  { path: "font.family.sans", type: "fontFamily", note: "every sans-serif text" },
  { path: "font.family.mono", type: "fontFamily", note: "every monospace text" },
  { path: "font.family.terminal", type: "fontFamily", note: "xterm's fontFamily" },
  { path: "duration.hover", type: "duration", note: "hover transitions of head chips" },
  { path: "duration.pulse", type: "duration", note: "the busy pulse" },
  { path: "duration.think", type: "duration", note: "the thinking shimmer" },
  { path: "easing.standard", type: "cubicBezier", note: "CSS ease" },
  { path: "easing.in-out", type: "cubicBezier", note: "CSS ease-in-out" },
  { path: "blur.veil", type: "dimension", note: "the stopped-terminal veil's backdrop blur" },
];

/** The schema by path. */
export const SCHEMA_BY_PATH: ReadonlyMap<string, SchemaToken> = new Map(SKIN_SCHEMA.map((t) => [t.path, t]));
