// Canvas compositor styles — ported from canvas_webapp/index.html's <style>
// block, minus the #bg WebGL layer (the GL host lands in a later spike; the
// first slice is the DOM compositor only). Injected by mountCanvas().

export const CANVAS_CSS = `
  html, body { margin: 0; padding: 0; height: 100%; background: #06060c; color: #aaa; font-family: system-ui, sans-serif; overflow: hidden; }
  #bg { position: fixed; inset: 0; width: 100vw; height: 100vh; z-index: 0; pointer-events: none; display: block; }
  #canvas { position: relative; width: 100%; height: 100vh; overflow: hidden; z-index: 1; }
  .canvas-world { position: absolute; inset: 0; transform-origin: 0 0; }
  /* its own layer only while it pans / zooms (canvas.ts moving) */
  #canvas.moving .canvas-world { will-change: transform; }
  #canvas.panning { cursor: grabbing; }
  #canvas.panning .agent-frame iframe,
  body.dragging-frame .agent-frame iframe { pointer-events: none; }

  .agent-frame {
    position: absolute;
    border-radius: 14px;
    overflow: hidden;
    display: flex;
    flex-direction: column;
    z-index: 2;
    /* a cell's paint stays inside it: its changes never invalidate the world */
    contain: layout style paint;
    background: rgba(15, 15, 25, 0.55);
    backdrop-filter: blur(16px) saturate(1.2);
    -webkit-backdrop-filter: blur(16px) saturate(1.2);
    border: 1px solid rgba(255, 255, 255, 0.08);
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.4), inset 0 1px 0 rgba(255, 255, 255, 0.04);
    transition: box-shadow .2s ease, border-color .2s ease, background .2s ease;
  }
  /* CONTENT ONLY until the pointer is on the cell (owner 2026-10-09): no bezel,
     no head, no handle; hovering (or dragging, resizing) shows them. The head
     floats OVER the content, so nothing reflows. A touch screen has no hover:
     there the chrome stays. */
  /* A TERMINAL'S SCROLLBAR only while the pointer is on its cell (owner
     2026-10-09, E65): xterm's scrollbar re-renders on every scrolled line, and
     40 streaming terminals with theirs shown held the page at ~7 fps (61 hidden).
     The wheel scrolls without it. */
  .agent-frame:not(:hover) .xterm .scrollbar,
  .agent-frame:not(:hover) .xterm-decoration-overview-ruler { display: none; }
  @media (hover: hover) {
    .agent-frame:not(:hover):not(.dragging):not(.resizing) {
      background: transparent; border-color: transparent; box-shadow: none;
      backdrop-filter: none; -webkit-backdrop-filter: none;
    }
    .agent-frame:not(:hover):not(.dragging):not(.resizing) > .frame-head,
    .agent-frame:not(:hover):not(.dragging):not(.resizing) > .resize-handle { opacity: 0; }
  }
  /* FE = page content, BE = a view of a host agent (backend-owned views) */
  .agent-frame[data-side="page"] { border-color: rgba(120, 170, 255, 0.28); }
  .agent-frame[data-side="host"] { border-color: rgba(255, 190, 90, 0.34); }
  .agent-frame:hover {
    border-color: rgba(255, 255, 255, 0.14);
    box-shadow: 0 8px 40px rgba(0, 0, 0, 0.5), inset 0 1px 0 rgba(255, 255, 255, 0.06);
  }
  .agent-frame.dragging, .agent-frame.resizing {
    border-color: rgba(255, 255, 255, 0.20);
    box-shadow: 0 12px 48px rgba(0, 0, 0, 0.6), inset 0 1px 0 rgba(255, 255, 255, 0.08);
  }
  .frame-head {
    position: absolute; top: 0; left: 0; right: 0; z-index: 4; box-sizing: border-box; min-height: 32px; padding: 9px 12px;
    background: rgba(15, 15, 25, 0.55);
    backdrop-filter: blur(12px) saturate(1.2); -webkit-backdrop-filter: blur(12px) saturate(1.2);
    transition: opacity .15s ease;
    display: flex; align-items: center; justify-content: space-between;
    border-bottom: 1px solid rgba(255, 255, 255, 0.05);
    color: rgba(255, 255, 255, 0.7);
    font: 600 11px ui-monospace, SFMono-Regular, monospace;
    letter-spacing: 0.5px; text-transform: uppercase; cursor: move; user-select: none;
  }
  .frame-head .id { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .frame-head .actions { display: flex; gap: 6px; align-items: center; flex: 0 0 auto; }
  .frame-head .reload, .frame-head .lock, .frame-head .close {
    display: inline-flex; align-items: center; justify-content: center;
    width: 20px; height: 20px; border-radius: 4px;
    background: rgba(255, 255, 255, 0.05); border: 1px solid rgba(255, 255, 255, 0.06);
    color: rgba(255, 255, 255, 0.7); font-size: 11px; cursor: pointer; user-select: none;
    transition: background .15s ease, color .15s ease;
  }
  .frame-head .reload:hover, .frame-head .lock:hover, .frame-head .close:hover {
    background: rgba(255, 255, 255, 0.12); color: rgba(255, 255, 255, 0.9);
  }
  .frame-head .lock.locked { color: #f7768e; background: rgba(247, 118, 142, 0.10); }
  .frame-body { position: relative; z-index: 2; flex: 1 1 0; min-height: 0; overflow: hidden; background: transparent; }
  .frame-body iframe { position: absolute; inset: 0; width: 100%; height: 100%; border: 0; background: transparent; }
  .resize-handle {
    position: absolute; bottom: 4px; right: 4px; width: 14px; height: 14px;
    cursor: nwse-resize;
    background: linear-gradient(135deg, transparent 50%, rgba(255,255,255,0.22) 50%);
    border-radius: 0 0 10px 0; z-index: 3; transition: opacity .15s ease;
  }
  /* THE SIDEBAR and CANVAS-LIST cells (side_view.ts): a cell's body, scrolling */
  .side { height: 100%; overflow-y: auto; box-sizing: border-box; padding: 8px 0 6px; display: flex; flex-direction: column;
    color: rgba(220,230,245,0.85); font: 12px ui-monospace, SFMono-Regular, monospace; }
  .side .chats-head { display: flex; justify-content: space-between; align-items: center; padding: 0 10px 6px 14px; color: rgba(220,230,245,0.55); }
  .side ul { list-style: none; margin: 0; padding: 0; }
  .side li { display: flex; gap: 8px; padding: 4px 14px; cursor: pointer; }
  .side li:hover { background: rgba(183,140,255,0.16); }
  .side li .nm { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .side li .ct { color: rgba(220,230,245,0.45); }
  .side li.canvas .dot { color: #ffd36b; } .side li.canvas.here { background: rgba(183,140,255,0.22); cursor: default; }
  .side .canvas-new { padding: 6px 14px 8px; color: #b78cff; cursor: pointer; }
  .side .canvas-new input { width: 100%; box-sizing: border-box; background: rgba(0,0,0,0.3); border: 1px solid rgba(183,140,255,0.4); color: inherit; font: inherit; padding: 3px 6px; border-radius: 6px; }
  .side .agents-tree li.agent { cursor: pointer; }
  .side .agents-tree li { gap: 6px; padding: 2px 10px; cursor: default; }
  .side .agents-tree li.kernel { color: rgba(220,230,245,0.55); padding: 6px 10px 2px; }
  .side .agents-tree li .tw { width: 10px; cursor: pointer; color: rgba(220,230,245,0.5); }
  .side .agents-tree li .gl { width: 12px; color: rgba(220,230,245,0.5); }
  .side .agents-tree li.agent { opacity: 0.6; }
  .side .agents-tree li.has-cell { opacity: 1; cursor: pointer; }
  .side .agents-tree li.has-cell .gl { color: #b78cff; }
  /* FE (page) and BE (host) in the frames' two colors; a NEW host agent stands out */
  .side .agents-tree li.agent.page .nm { color: rgba(140, 185, 255, 0.95); }
  .side .agents-tree li.agent.host .nm { color: rgba(255, 200, 120, 0.95); }
  .side .agents-tree li.fresh { opacity: 1; }
  .side .agents-tree li .del { display: none; margin-left: auto; padding: 0 4px; color: rgba(255, 140, 140, 0.8); cursor: pointer; }
  .side .agents-tree li:hover .del { display: inline; }
  .side .agents-tree li.fresh .ct { color: #ffd166; font-weight: 600; }
  .side .agents-tree li.kernel:hover, .side .agents-tree li.agent:not(.has-cell):hover { background: transparent; }
  .agent-frame.flash { box-shadow: 0 0 0 2px #b78cff, 0 0 30px rgba(183,140,255,0.6) !important; }
  .canvas-toast { position: fixed; bottom: 18px; left: 50%; transform: translateX(-50%); z-index: 1100;
    background: rgba(40, 20, 26, 0.9); color: #ffb3b3; padding: 8px 14px; border-radius: 10px;
    font: 13px ui-monospace, SFMono-Regular, monospace; box-shadow: 0 0 0 1px rgba(255,120,120,0.3); }
  /* VIEWING ONE AGENT FULL: no world transform, the frame fills the page */
  #canvas.full-view .canvas-world { transform: none !important; }
  #canvas.full-view .agent-frame.full { left: 12px !important; top: 12px !important;
    width: calc(100vw - 24px) !important; height: calc(100vh - 24px) !important; }
  #canvas.full-view .agent-frame.full .resize-handle { display: none; }
  .side .agent.viewed { background: rgba(183,140,255,0.18); }
`;
