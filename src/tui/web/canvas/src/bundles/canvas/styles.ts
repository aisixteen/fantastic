// Canvas compositor styles — ported from canvas_webapp/index.html's <style>
// block, minus the #bg WebGL layer (the GL host lands in a later spike; the
// first slice is the DOM compositor only). Injected by mountCanvas().

import { tint, tok } from "../skin/css.ts";

export const CANVAS_CSS = `
  html, body { margin: 0; padding: 0; height: 100%; background: ${tok("color.page.bg")}; color: ${tok("color.page.text")}; font-family: ${tok("font.family.sans")}; overflow: hidden; }
  #bg { position: fixed; inset: 0; width: 100vw; height: 100vh; z-index: 0; pointer-events: none; display: block; }
  #canvas { position: relative; width: 100%; height: 100vh; overflow: hidden; z-index: 1; }
  .canvas-world { position: absolute; inset: 0; transform-origin: 0 0; }
  /* its own layer only while it pans / zooms (canvas.ts moving) */
  #canvas.moving .canvas-world { will-change: transform; }
  #canvas.panning { cursor: grabbing; }
  /* a tree drag over the canvas: the drop highlight (#134, #135) */
  #canvas.drop-target { box-shadow: inset 0 0 0 2px ${tint("color.accent.base", 55)}; }
  #canvas.panning .agent-frame iframe,
  body.dragging-frame .agent-frame iframe { pointer-events: none; }

  /* A CELL IS ITS CONTENT; ITS CHROME GROWS OUTWARD (owner 2026-10-09): the
     frame's box is exactly the content's, and the bezel (glass, border, shadow —
     \`::before\`), the head (above) and the resize handle (the corner) sit OUTSIDE
     it, shown while the pointer is on the cell (or it is dragged / resized). The
     content never shrinks, moves or is covered. A touch screen has no hover:
     there the chrome stays. */
  .agent-frame {
    position: absolute;
    display: flex;
    flex-direction: column;
    z-index: 2;
    /* a cell's layout stays inside it (no paint containment: the chrome is outside) */
    contain: layout style;
  }
  .agent-frame::before {
    content: ""; position: absolute; inset: -32px -6px -6px -6px; z-index: -1;
    border-radius: ${tok("radius.bezel")};
    /* NO backdrop blur (owner 2026-10-11, E70): a blurred bezel re-filtered the
       animated background every frame — one hovered cell took the GPU process
       from 27% to 440% CPU on desktop, every cell on mobile 185% → 658% */
    background: ${tok("color.surface.bezel")};
    border: 1px solid ${tint("color.ink", 10)};
    box-shadow: ${tok("shadow.bezel")};
    /* NO TRANSITIONS on the chrome (E73): a fade re-painted the bezel's shadow
       every frame as the pointer crossed cells — the GPU process 125% → 511% */
  }
  /* A TERMINAL'S SCROLLBAR only while the pointer is on its cell (owner
     2026-10-09, E65): xterm's scrollbar re-renders on every scrolled line, and
     40 streaming terminals with theirs shown held the page at ~7 fps (61 hidden).
     The wheel scrolls without it. */
  .agent-frame:not(:hover) .xterm .scrollbar,
  .agent-frame:not(:hover) .xterm-decoration-overview-ruler { display: none; }
  @media (hover: hover) {
    /* hidden, the bezel still catches the pointer: coming at a cell from outside,
       its chrome shows on its REAL border (owner 2026-10-11), not only once the
       pointer is over the content */
    .agent-frame:not(:hover):not(.dragging):not(.resizing)::before { opacity: 0; }
    .agent-frame:not(:hover):not(.dragging):not(.resizing) > .frame-head,
    .agent-frame:not(:hover):not(.dragging):not(.resizing) > .resize-handle { opacity: 0; pointer-events: none; }
  }
  /* a margin round the bezel that also counts as on the cell: a little above or
     beside it shows the chrome — and its corners to resize from */
  .agent-frame::after { content: ""; position: absolute; inset: -42px -16px -16px -16px; z-index: -2; }
  /* FE = page content, BE = a view of a host agent (backend-owned views) */
  .agent-frame[data-side="page"]::before { border-color: ${tint("color.side.page", 28)}; }
  .agent-frame[data-side="host"]::before { border-color: ${tint("color.side.host", 34)}; }
  .agent-frame.dragging::before, .agent-frame.resizing::before {
    box-shadow: ${tok("shadow.bezel-lifted")};
  }
  .frame-head {
    position: absolute; top: -32px; left: 0; right: 0; height: 32px; z-index: 4; box-sizing: border-box; padding: 0 6px;
    display: flex; align-items: center; justify-content: space-between;
    color: ${tint("color.ink", 70)};
    font: 600 11px ${tok("font.family.mono")};
    letter-spacing: 0.5px; text-transform: uppercase; cursor: move; user-select: none;
  }
  .frame-head .id { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .frame-head .actions { display: flex; gap: 6px; align-items: center; flex: 0 0 auto; }
  .frame-head .reload, .frame-head .lock, .frame-head .min, .frame-head .max, .frame-head .close {
    display: inline-flex; align-items: center; justify-content: center;
    width: 20px; height: 20px; border-radius: ${tok("radius.chip")};
    background: ${tint("color.ink", 5)}; border: 1px solid ${tint("color.ink", 6)};
    color: ${tint("color.ink", 70)}; font-size: 11px; cursor: pointer; user-select: none;
    transition: background ${tok("duration.hover")} ${tok("easing.standard")}, color ${tok("duration.hover")} ${tok("easing.standard")};
  }
  /* A VIEW'S OWN CHIPS (⚙ settings…): bigger than the frame's, easy to hit (owner 2026-10-11) */
  .frame-head .ext {
    display: inline-flex; align-items: center; justify-content: center;
    min-width: 26px; height: 24px; padding: 0 4px; box-sizing: border-box; border-radius: ${tok("radius.control")};
    background: ${tint("color.accent.base", 16)}; border: 1px solid ${tint("color.accent.base", 45)};
    color: ${tok("color.accent.text")}; font-size: 16px; line-height: 1; cursor: pointer; user-select: none;
  }
  .frame-head .ext:hover { background: ${tint("color.accent.base", 32)}; color: ${tok("color.text.bright")}; }
  .frame-head .ext.active { background: ${tint("color.accent.base", 45)}; color: ${tok("color.text.bright")}; }
  .frame-head .reload:hover, .frame-head .lock:hover, .frame-head .min:hover, .frame-head .max:hover, .frame-head .close:hover {
    background: ${tint("color.ink", 12)}; color: ${tint("color.ink", 90)};
  }
  .frame-head .lock.locked { color: ${tok("color.status.lock")}; background: ${tint("color.status.lock", 10)}; }
  .frame-head .pin { display: none; }
  #canvas.theme-mobile .frame-head .pin {
    display: inline-flex; align-items: center; justify-content: center;
    width: 20px; height: 20px; border-radius: ${tok("radius.chip")}; font-size: 11px; cursor: pointer; user-select: none;
    background: ${tint("color.ink", 5)}; border: 1px solid ${tint("color.ink", 6)}; filter: grayscale(1); opacity: .7;
  }
  #canvas.theme-mobile .frame-head .pin.on { filter: none; opacity: 1; background: ${tint("color.status.ok", 12)}; border-color: ${tint("color.status.ok", 40)}; }
  .agent-frame.pinned { z-index: 50; }
  /* MOBILE: the chrome always shows (no hover on a phone), the bezel opaque over
     the background's headline; the width is the column's — no resize */
  #canvas.theme-mobile .agent-frame::before,
  #canvas.theme-mobile .agent-frame > .frame-head { opacity: 1; pointer-events: auto; }
  #canvas.theme-mobile .agent-frame::before { background: ${tok("color.surface.bezel-flat")}; }
  #canvas.theme-mobile .agent-frame > .resize-handle { display: none; }
  /* MOBILE IS FLAT (owner 2026-10-12): no border, no rounding, edge to edge; the
     head's middle is a grip that drags a cell up or down the column */
  #canvas.theme-mobile .agent-frame::before { inset: -32px 0 0 0; border: 0; border-radius: 0; box-shadow: none; }
  #canvas.theme-mobile .agent-frame::after { inset: -32px 0 0 0; }
  #canvas.theme-mobile .agent-frame > .frame-body { border-radius: 0; }
  #canvas.theme-mobile .agent-frame.dragging { z-index: 40; }
  #canvas.theme-mobile .agent-frame > .frame-head { cursor: default; }
  /* the name stops short of the grip in the head's middle */
  #canvas.theme-mobile .frame-head .id { max-width: calc(50% - 36px); }
  .frame-head .grip { display: none; }
  #canvas.theme-mobile .frame-head .grip {
    display: inline-flex; align-items: center; justify-content: center;
    position: absolute; left: 50%; top: 0; transform: translateX(-50%);
    width: 56px; height: 32px; font-size: 18px; color: ${tint("color.ink", 55)};
    cursor: grab; touch-action: none; user-select: none;
  }
  #canvas.theme-mobile .agent-frame.dragging .frame-head .grip { cursor: grabbing; color: ${tok("color.text.bright")}; }
  /* MINIMIZED (owner 2026-10-12): the head bar alone — the body stays mounted at
     its size (a terminal keeps its rows), only hidden; the bezel shrinks to the bar */
  .agent-frame.minimized > .frame-body,
  .agent-frame.minimized > .resize-handle:not(.ne):not(.nw) { visibility: hidden; pointer-events: none; }
  /* the bar is all there is of it: shown without hover, or the cell vanishes on desktop */
  .agent-frame.minimized::before { opacity: 1 !important; }
  .agent-frame.minimized > .frame-head { opacity: 1 !important; pointer-events: auto !important; }
  .agent-frame.minimized::before { bottom: auto; height: 32px; }
  .agent-frame.minimized::after { bottom: auto; height: 42px; }
  #canvas.theme-mobile .agent-frame.minimized::before,
  #canvas.theme-mobile .agent-frame.minimized::after { bottom: auto; height: 32px; }
  .frame-body { position: relative; z-index: 2; flex: 1 1 0; min-height: 0; overflow: hidden; background: transparent; border-radius: ${tok("radius.panel")}; }
  .frame-body iframe { position: absolute; inset: 0; width: 100%; height: 100%; border: 0; background: transparent; }
  /* A HANDLE ON EACH CORNER of the bezel (head included) */
  .resize-handle { position: absolute; width: 16px; height: 16px; z-index: 5; }
  .resize-handle.se { bottom: -8px; right: -8px; cursor: nwse-resize; border-radius: 0 0 ${tok("radius.handle")} 0;
    background: linear-gradient(135deg, transparent 50%, ${tint("color.ink", 30)} 50%); }
  .resize-handle.sw { bottom: -8px; left: -8px; cursor: nesw-resize; border-radius: 0 0 0 ${tok("radius.handle")};
    background: linear-gradient(225deg, transparent 50%, ${tint("color.ink", 30)} 50%); }
  .resize-handle.ne { top: -40px; right: -8px; cursor: nesw-resize; border-radius: 0 ${tok("radius.handle")} 0 0;
    background: linear-gradient(45deg, transparent 50%, ${tint("color.ink", 30)} 50%); }
  .resize-handle.nw { top: -40px; left: -8px; cursor: nwse-resize; border-radius: ${tok("radius.handle")} 0 0 0;
    background: linear-gradient(315deg, transparent 50%, ${tint("color.ink", 30)} 50%); }
  /* A TERMINAL'S LAST LAUNCHED COMMAND (owner 2026-10-12): a bar on the cell's
     top right; after a restart its ▶ run types it into the new shell */
  .term-last { position: absolute; top: 6px; right: 14px; z-index: 3; display: flex; align-items: center; gap: 6px;
    max-width: calc(100% - 40px); padding: 2px 4px 2px 8px; border-radius: ${tok("radius.control")}; background: ${tok("color.surface.badge")};
    border: 1px solid ${tint("color.ink", 8)}; font: 11px ${tok("font.family.mono")}; color: ${tint("color.ink", 55)};
    pointer-events: none; }
  .term-last[hidden] { display: none; }
  .term-last .cmd { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
  .term-last .cmd::before { content: "↺ "; opacity: .6; }
  /* STOPPED (owner 2026-10-12): no live job past its first start — nothing starts
     on its own; the human's ▶ does */
  .term-stopped { position: absolute; inset: 0; z-index: 4; display: flex; align-items: center; justify-content: center;
    background: ${tok("color.surface.veil")}; backdrop-filter: blur(${tok("blur.veil")}); }
  .term-stopped[hidden] { display: none; }
  .term-stopped .box { max-width: 80%; padding: 14px 16px; border-radius: ${tok("radius.panel")}; background: ${tok("color.surface.raised")};
    border: 1px solid ${tint("color.ink", 10)}; color: ${tok("color.terminal.fg")}; font: 12.5px/1.4 ${tok("font.family.sans")}; text-align: center; }
  .term-stopped .title { font-weight: 650; font-size: 13.5px; margin-bottom: 4px; }
  .term-stopped .note { color: ${tint("color.terminal.fg", 62)}; margin-bottom: 10px; }
  .term-stopped .row { display: flex; gap: 8px; justify-content: center; flex-wrap: wrap; }
  .term-stopped button { font: 600 12px ${tok("font.family.mono")}; color: ${tok("color.status.ok-on")}; background: ${tok("color.status.ok")}; border: 0;
    border-radius: ${tok("radius.control")}; padding: 5px 12px; cursor: pointer; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .term-stopped button:hover { background: ${tok("color.status.ok-hover")}; }
  .term-stopped button[hidden] { display: none; }
  /* THE CANVAS-LIST cell (side_view.ts), and the base of the file tree (files/styles.ts): a cell's body, scrolling */
  .side { height: 100%; overflow-y: auto; box-sizing: border-box; padding: 8px 0 6px; display: flex; flex-direction: column;
    color: ${tint("color.text.cool", 85)}; font: 12px ${tok("font.family.mono")}; }
  .side .chats-head { display: flex; justify-content: space-between; align-items: center; padding: 0 10px 6px 14px; color: ${tint("color.text.cool", 55)}; }
  .side ul { list-style: none; margin: 0; padding: 0; }
  .side li { display: flex; gap: 8px; padding: 4px 14px; cursor: pointer; }
  .side li:hover { background: ${tint("color.accent.base", 16)}; }
  .side li .nm { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .side li .ct { color: ${tint("color.text.cool", 45)}; }
  .side li.canvas .dot { color: ${tok("color.status.gold")}; } .side li.canvas.here { background: ${tint("color.accent.base", 22)}; cursor: default; }
  .side .canvas-new { padding: 6px 14px 8px; color: ${tok("color.accent.base")}; cursor: pointer; }
  .side .canvas-new input { width: 100%; box-sizing: border-box; background: ${tint("color.shade", 30)}; border: 1px solid ${tint("color.accent.base", 40)}; color: inherit; font: inherit; padding: 3px 6px; border-radius: ${tok("radius.control")}; }
  .agent-frame.flash { box-shadow: ${tok("shadow.flash")} !important; }
  .canvas-toast { position: fixed; bottom: 18px; left: 50%; transform: translateX(-50%); z-index: 1100;
    background: ${tok("color.surface.toast")}; color: ${tok("color.status.error-text")}; padding: 8px 14px; border-radius: ${tok("radius.panel")};
    font: 13px ${tok("font.family.mono")}; box-shadow: ${tok("shadow.toast")}; }
  /* VIEWING ONE AGENT FULL: no world transform, the frame fills the page */
  #canvas.full-view .canvas-world { transform: none !important; }
  #canvas.full-view .agent-frame.full { left: 12px !important; top: 44px !important;
    width: calc(100vw - 24px) !important; height: calc(100vh - 56px) !important; }
  #canvas.full-view .agent-frame.full .resize-handle { display: none; }
  /* an AI cell not set up yet (the wizard's skip) */
  .ai-not-set-up { height: 100%; box-sizing: border-box; display: flex; flex-direction: column; align-items: center; justify-content: center;
    gap: 10px; padding: 24px; text-align: center; background: ${tok("color.surface.panel")}; color: ${tok("color.text.muted")}; border-radius: ${tok("radius.panel")};
    font: 14px/1.5 ${tok("font.family.sans")}; }
  .ai-not-set-up .nsu-title { color: ${tok("color.text.primary")}; font-size: 17px; font-weight: 650; }
  .ai-not-set-up p { margin: 0; max-width: 300px; }
  .ai-not-set-up .nsu-row { display: flex; gap: 8px; margin-top: 6px; }
  .ai-not-set-up button { font: inherit; color: ${tok("color.text.primary")}; background: ${tok("color.surface.button")}; border: 1px solid ${tok("color.line.button")}; border-radius: ${tok("radius.card")}; padding: 8px 14px; cursor: pointer; }
  .ai-not-set-up button:hover { border-color: ${tok("color.accent.base")}; }
  .ai-not-set-up .nsu-settings { background: ${tok("color.accent.base")}; border-color: ${tok("color.accent.base")}; color: ${tok("color.accent.on")}; font-weight: 700; }
  /* the viewed-full label: centred ON the frame's top border (top: 44px) */
  .full-hint { position: fixed; top: 44px; left: 50%; transform: translate(-50%, -50%); z-index: 60;
    max-width: calc(100vw - 48px); box-sizing: border-box; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
    padding: 7px 16px; border-radius: ${tok("radius.pill")}; pointer-events: none;
    background: ${tok("color.surface.pill")}; box-shadow: ${tok("shadow.pill")};
    color: ${tint("color.text.cool", 72)}; font: 400 13px/1.2 ${tok("font.family.sans")}; letter-spacing: 0.2px; }
  .full-hint[hidden] { display: none; }
  /* THE TREE SIDEBAR (#148): beside the zoomed world, fixed to the viewport's left edge — a zoom
     or a pan never moves or scales it. Over the canvas on a narrow screen (an overlay there). */
  .tree-sidebar { position: fixed; top: 0; bottom: 0; left: 0; z-index: 2; display: flex; box-sizing: border-box; width: 300px; max-width: 80vw;
    background: ${tok("color.surface.panel")}; border-right: 1px solid ${tint("color.accent.base", 30)}; }
  .tree-sidebar.collapsed { width: 22px; }
  .tree-sidebar .ts-toggle { flex: none; width: 22px; height: 100%; padding: 0; border: 0; background: transparent; cursor: pointer;
    color: ${tint("color.text.cool", 60)}; font: 14px ${tok("font.family.mono")}; }
  .tree-sidebar .ts-toggle:hover { color: ${tok("color.text.bright")}; background: ${tint("color.accent.base", 14)}; }
  .tree-sidebar .ts-body { flex: 1; min-width: 0; display: flex; }
  .tree-sidebar.collapsed .ts-body { display: none; }
  .tree-sidebar .ts-body > .files-view { flex: 1; min-width: 0; }
  .full-hint b { color: ${tok("color.text.bright")}; font-weight: 600; }
  .full-hint kbd { font: 600 11px ${tok("font.family.mono")}; color: ${tok("color.accent.text")};
    padding: 2px 6px; margin: 0 1px; border-radius: ${tok("radius.key")}; background: ${tint("color.accent.base", 16)}; box-shadow: inset 0 0 0 1px ${tint("color.accent.base", 40)}; }
  .side .agent.viewed { background: ${tint("color.accent.base", 18)}; }
  /* A CELL'S HEAD MENU (right-click on the head, #133): Hide, Delete */
  .cell-menu { position: fixed; z-index: 1200; display: flex; flex-direction: column; min-width: 120px; padding: 4px;
    border-radius: ${tok("radius.control")}; background: ${tok("color.surface.raised")}; border: 1px solid ${tint("color.ink", 10)};
    box-shadow: ${tok("shadow.toast")}; }
  .cell-menu button { font: 12.5px ${tok("font.family.sans")}; text-align: left; color: ${tok("color.text.primary")}; background: transparent;
    border: 0; border-radius: ${tok("radius.control")}; padding: 5px 10px; cursor: pointer; }
  .cell-menu button:hover { background: ${tint("color.accent.base", 16)}; }
  .cell-menu button[data-intent="delete"] { color: ${tok("color.status.error-text")}; }
`;
