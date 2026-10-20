import { tint, tok } from "../skin/css.ts";

// THE FILE TREE'S STYLE — every colour, radius and font through the skin's
// tokens. Injected once, with the first tree.

const CSS = `
  .files-view { height: 100%; box-sizing: border-box; padding: 8px 0 0; display: flex; flex-direction: column; overflow: hidden;
    color: ${tint("color.text.cool", 85)}; font: 12px ${tok("font.family.mono")}; }
  .files-view .ft-head { display: flex; flex-wrap: wrap; align-items: center; gap: 4px 12px; padding: 0 10px 6px; color: ${tint("color.text.cool", 55)}; }
  .files-view .ft-kernel.host { color: ${tok("color.side.host-text")}; }
  .files-view .ft-kernel.page { color: ${tok("color.side.page-text")}; }
  .files-view .ft-new { color: ${tok("color.status.warn")}; font-weight: 600; cursor: pointer; }
  .files-view .ft-new[hidden] { display: none; }
  .files-view .ft-filter { flex: 1 1 100%; box-sizing: border-box; background: ${tint("color.shade", 30)}; border: 1px solid ${tint("color.accent.base", 40)};
    color: inherit; font: inherit; padding: 3px 6px; border-radius: ${tok("radius.control")}; }
  .files-view ul.ft-tree { flex: 1; min-height: 0; overflow-y: auto; list-style: none; margin: 0; padding: 0 0 6px; outline: none; }
  .files-view .ft-row { display: flex; align-items: center; gap: 6px; padding: 2px 10px; cursor: pointer; white-space: nowrap; }
  .files-view .ft-row:hover { background: ${tint("color.accent.base", 16)}; }
  .files-view .ft-row.sel { background: ${tint("color.accent.base", 22)}; }
  .files-view ul.ft-tree:focus-visible .ft-row.cur { box-shadow: inset 0 0 0 1px ${tint("color.accent.base", 55)}; }
  .files-view .ft-row .tw { width: 10px; cursor: pointer; color: ${tint("color.text.cool", 50)}; }
  .files-view .ft-row .gl { width: 14px; text-align: center; color: ${tint("color.text.cool", 50)}; }
  .files-view .ft-row .nm { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
  .files-view .ft-row .nm .id { color: ${tint("color.text.cool", 45)}; }
  .files-view .ft-row .ct, .files-view .ft-row .st { color: ${tint("color.text.cool", 45)}; }
  .files-view .ft-row .go, .files-view .ft-row .del { display: none; padding: 0 4px; cursor: pointer; }
  .files-view .ft-row .go { color: ${tok("color.accent.base")}; }
  .files-view .ft-row .del { color: ${tint("color.status.error", 80)}; }
  .files-view .ft-row:hover .go, .files-view .ft-row:hover .del { display: inline; }
  .files-view .ft-row.elsewhere .go { display: none !important; }
  /* the project's own folder, in its accent */
  .files-view .ft-row.fantastic { background: ${tint("color.tree.fantastic", 10)}; }
  .files-view .ft-row.fantastic .nm, .files-view .ft-row.fantastic .gl { color: ${tok("color.tree.fantastic")}; }
  /* an agent folder: FE (page) and BE (host) in the frames' two colours */
  .files-view .ft-row.agent.page .nm { color: ${tok("color.side.page-text")}; }
  .files-view .ft-row.agent.host .nm { color: ${tok("color.side.host-text")}; }
  .files-view .ft-row.agent.unloaded { opacity: 0.7; }
  .files-view .ft-row.agent.elsewhere { opacity: 0.45; }
  .files-view .ft-row.agent.has-cell .gl { color: ${tok("color.accent.base")}; }
  .files-view .ft-row.agent.viewed .nm { font-weight: 600; }
  .files-view .ft-row.fresh .ct { color: ${tok("color.status.warn")}; font-weight: 600; }
  .files-view .ft-row.err .st { color: ${tok("color.status.error")}; }
  .files-view .ft-group { padding: 6px 10px 2px; color: ${tint("color.text.cool", 55)}; cursor: default; }
`;

export function ensureFilesStyle(): void {
  if (document.getElementById("fantastic-files-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-files-style";
  s.textContent = CSS;
  document.head.appendChild(s);
}
