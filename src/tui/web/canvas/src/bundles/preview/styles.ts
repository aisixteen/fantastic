import { tint, tok } from "../skin/css.ts";

// THE PREVIEWER'S STYLE — every colour, radius, font and duration through the
// skin's tokens. Injected once, with the first preview.

const CSS = `
  .preview-view { height: 100%; box-sizing: border-box; display: flex; flex-direction: column; overflow: hidden;
    color: ${tint("color.text.cool", 85)}; font: 12px ${tok("font.family.mono")}; background: ${tok("color.surface.well")}; }
  .preview-view .pv-bar { display: flex; align-items: center; gap: 6px; padding: 5px 8px; flex: none; border-bottom: 1px solid ${tok("color.line.faint")}; }
  .preview-view .pv-title { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: ${tint("color.text.cool", 75)}; }
  .preview-view .pv-live { color: ${tok("color.status.ok")}; }
  .preview-view .pv-unsaved { color: ${tok("color.status.warn")}; }
  .preview-view .pv-chip { flex: none; cursor: pointer; padding: 1px 7px; border: 1px solid ${tok("color.line.button")}; border-radius: ${tok("radius.chip")};
    background: ${tok("color.surface.button")}; color: ${tok("color.text.primary")}; font: inherit; transition: background ${tok("duration.hover")}; }
  .preview-view .pv-chip:hover { background: ${tint("color.accent.base", 16)}; border-color: ${tok("color.line.strong")}; }
  .preview-view .pv-chip.active { background: ${tint("color.accent.base", 26)}; }
  .preview-view .pv-chip.off { opacity: 0.45; cursor: not-allowed; }
  .preview-view .pv-note { flex: none; padding: 4px 10px; color: ${tok("color.status.warn-text")}; background: ${tint("color.status.warn", 10)}; }
  .preview-view .pv-note[hidden] { display: none; }
  .preview-view .pv-conflict { flex: none; display: flex; flex-wrap: wrap; align-items: center; gap: 6px 10px; padding: 6px 10px;
    color: ${tok("color.status.warn-text")}; background: ${tint("color.status.warn", 16)}; border-bottom: 1px solid ${tok("color.status.warn-line")}; }
  .preview-view .pv-conflict .msg { flex: 1 1 220px; }
  .preview-view .pv-body { flex: 1; min-height: 0; position: relative; overflow: hidden; display: flex; flex-direction: column; }
  .preview-view .pv-empty { padding: 16px; color: ${tok("color.text.faint")}; }
  .preview-view .pv-error { padding: 16px; color: ${tok("color.status.error-text")}; white-space: pre-wrap; word-break: break-word; }
  .preview-view .pv-card { padding: 16px; line-height: 1.6; }
  .preview-view .pv-card .big { color: ${tok("color.text.bright")}; }
  /* text: a gutter and the highlighted source; the editor lays a textarea over the same box */
  .preview-view .pv-scroll { flex: 1; min-height: 0; display: grid; grid-template-columns: auto 1fr; overflow: auto; align-content: start; }
  .preview-view .pv-gutter, .preview-view .pv-code, .preview-view .pv-input { margin: 0; padding: 8px 10px; font: 12px/1.5 ${tok("font.family.mono")}; tab-size: 2; }
  .preview-view .pv-gutter { position: sticky; left: 0; text-align: right; user-select: none; color: ${tok("color.text.faint")}; background: ${tok("color.surface.well")}; padding-right: 8px; }
  .preview-view .pv-code { white-space: pre; color: ${tok("color.text.code")}; }
  .preview-view .pv-stack { position: relative; min-width: max-content; }
  .preview-view .pv-input { position: absolute; inset: 0; width: 100%; height: 100%; box-sizing: border-box; resize: none; overflow: hidden; border: 0; outline: none;
    white-space: pre; color: transparent; background: transparent; caret-color: ${tok("color.text.bright")}; }
  .preview-view .pv-input::selection { background: ${tint("color.accent.base", 35)}; }
  .preview-view .pv-input[readonly] { cursor: default; }
  .preview-view .tk-kw { color: ${tok("color.syntax.keyword")}; }
  .preview-view .tk-str { color: ${tok("color.syntax.string")}; }
  .preview-view .tk-com { color: ${tok("color.syntax.comment")}; }
  .preview-view .tk-num { color: ${tok("color.syntax.number")}; }
  .preview-view .tk-type { color: ${tok("color.syntax.type")}; }
  .preview-view .tk-punct { color: ${tok("color.syntax.punct")}; }
  /* compare: mine and the disk's, side by side */
  .preview-view .pv-diff { flex: 1; min-height: 0; display: grid; grid-template-columns: 1fr 1fr; overflow: auto; align-content: start; }
  .preview-view .pv-diff h4 { margin: 0; padding: 4px 10px; font-weight: 600; color: ${tok("color.text.muted")}; position: sticky; top: 0; background: ${tok("color.surface.well")}; }
  .preview-view .pv-diff pre { margin: 0; padding: 0 10px; white-space: pre; font: 12px/1.5 ${tok("font.family.mono")}; }
  .preview-view .pv-diff .del { background: ${tint("color.status.error", 18)}; }
  .preview-view .pv-diff .add { background: ${tint("color.status.ok", 18)}; }
  .preview-view .pv-diff .blank { opacity: 0.4; }
  /* a folder's entries */
  .preview-view .pv-dir { flex: 1; overflow: auto; padding: 6px 0; }
  .preview-view .pv-dir .e { display: flex; gap: 10px; padding: 2px 12px; }
  .preview-view .pv-dir .e .n { flex: 1; }
  .preview-view .pv-dir .e .s { color: ${tint("color.text.cool", 45)}; }
  .preview-view .pv-img { flex: 1; min-height: 0; display: grid; place-items: center; overflow: auto; padding: 8px; }
  .preview-view .pv-img img { max-width: 100%; max-height: 100%; object-fit: contain; }
  .preview-view object.pv-pdf { flex: 1; width: 100%; border: 0; }
  .preview-view .pv-pager { display: flex; align-items: center; gap: 8px; padding: 6px 10px; flex: none; }
  /* markdown, rendered */
  .preview-view .pv-md { flex: 1; overflow: auto; padding: 10px 16px; font: 13px/1.55 ${tok("font.family.sans")}; color: ${tok("color.text.primary")}; }
  .preview-view .pv-md h1, .preview-view .pv-md h2, .preview-view .pv-md h3, .preview-view .pv-md h4 { color: ${tok("color.text.bright")}; margin: 1em 0 0.4em; }
  .preview-view .pv-md a { color: ${tok("color.accent.soft")}; cursor: pointer; text-decoration: underline; }
  .preview-view .pv-md code { font-family: ${tok("font.family.mono")}; background: ${tok("color.surface.code")}; padding: 0 4px; border-radius: ${tok("radius.tag")}; }
  .preview-view .pv-md pre { background: ${tok("color.surface.code")}; padding: 8px 10px; border-radius: ${tok("radius.card")}; overflow: auto; }
  .preview-view .pv-md pre code { padding: 0; background: transparent; }
  .preview-view .pv-md blockquote { margin: 0.6em 0; padding-left: 12px; border-left: 3px solid ${tok("color.line.strong")}; color: ${tok("color.text.muted")}; }
  .preview-view .pv-md table { border-collapse: collapse; }
  .preview-view .pv-md th, .preview-view .pv-md td { border: 1px solid ${tok("color.line.button")}; padding: 3px 8px; }
  .preview-view .pv-md img { max-width: 100%; }
  .preview-view .pv-md hr { border: 0; border-top: 1px solid ${tok("color.line.faint")}; }
  /* an agent's page reflect card */
  .preview-view .pv-agent { flex: 1; min-height: 0; overflow: auto; position: relative; }
  .preview-view .reflect { padding: 12px 16px; line-height: 1.5; }
  .preview-view .reflect h3 { margin: 0 0 6px; color: ${tok("color.text.bright")}; }
  .preview-view .reflect pre { margin: 6px 0; padding: 8px 10px; overflow: auto; background: ${tok("color.surface.code")}; border-radius: ${tok("radius.card")}; color: ${tok("color.text.code")}; }
  .preview-view .reflect .acts { display: flex; gap: 8px; margin-top: 10px; }
`;

export function ensurePreviewStyle(): void {
  if (document.getElementById("fantastic-preview-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-preview-style";
  s.textContent = CSS;
  document.head.appendChild(s);
}
