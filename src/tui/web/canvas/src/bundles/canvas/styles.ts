// Canvas compositor styles — ported from canvas_webapp/index.html's <style>
// block, minus the #bg WebGL layer (the GL host lands in a later spike; the
// first slice is the DOM compositor only). Injected by mountCanvas().

export const CANVAS_CSS = `
  /* a canvas this tab's user may not write: its editing controls are gone */
  body.ro .agent-frame .close, body.ro .agent-frame .lock, body.ro .agent-frame .resize-handle,
  body.ro .setup-link, body.ro .chat-view .c-row, body.ro .chat-view .c-main, body.ro .chat-view .c-keep { display: none !important; }
  html, body { margin: 0; padding: 0; height: 100%; background: #06060c; color: #aaa; font-family: system-ui, sans-serif; overflow: hidden; }
  #bg { position: fixed; inset: 0; width: 100vw; height: 100vh; z-index: 0; pointer-events: none; display: block; }
  #canvas { position: relative; width: 100%; height: 100vh; overflow: hidden; z-index: 1; }
  .canvas-world { position: absolute; inset: 0; transform-origin: 0 0; will-change: transform; }
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
    background: rgba(15, 15, 25, 0.55);
    backdrop-filter: blur(16px) saturate(1.2);
    -webkit-backdrop-filter: blur(16px) saturate(1.2);
    border: 1px solid rgba(255, 255, 255, 0.08);
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.4), inset 0 1px 0 rgba(255, 255, 255, 0.04);
    transition: box-shadow .3s ease, border-color .3s ease;
  }
  .agent-frame:hover {
    border-color: rgba(255, 255, 255, 0.14);
    box-shadow: 0 8px 40px rgba(0, 0, 0, 0.5), inset 0 1px 0 rgba(255, 255, 255, 0.06);
  }
  .agent-frame.dragging, .agent-frame.resizing {
    border-color: rgba(255, 255, 255, 0.20);
    box-shadow: 0 12px 48px rgba(0, 0, 0, 0.6), inset 0 1px 0 rgba(255, 255, 255, 0.08);
  }
  .frame-head {
    position: relative; z-index: 2; min-height: 32px; padding: 9px 12px;
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
  .frame-body { position: relative; z-index: 2; flex: 1; background: transparent; }
  .frame-body iframe { position: absolute; inset: 0; width: 100%; height: 100%; border: 0; background: transparent; }
  .resize-handle {
    position: absolute; bottom: 4px; right: 4px; width: 14px; height: 14px;
    cursor: nwse-resize;
    background: linear-gradient(135deg, transparent 50%, rgba(255,255,255,0.22) 50%);
    border-radius: 0 0 10px 0; z-index: 3;
  }
  #toolbar {
    position: fixed; top: 12px; left: 12px; padding: 8px 14px; border-radius: 999px;
    background: rgba(20, 22, 36, 0.55);
    backdrop-filter: blur(24px) saturate(180%);
    -webkit-backdrop-filter: blur(24px) saturate(180%);
    box-shadow: inset 0 1px 0 rgba(255,255,255,0.10), 0 0 0 1px rgba(255,255,255,0.10), 0 12px 32px -8px rgba(0,0,0,0.55);
    color: rgba(220,230,245,0.85); font: 500 12px ui-monospace, SFMono-Regular, monospace; z-index: 100;
  }
  #chats { position: fixed; top: 56px; left: 12px; width: 220px; z-index: 90; padding: 8px 0 6px;
    background: rgba(20, 22, 36, 0.55); backdrop-filter: blur(24px) saturate(180%); -webkit-backdrop-filter: blur(24px) saturate(180%);
    border-radius: 12px; box-shadow: inset 0 1px 0 rgba(255,255,255,0.10), 0 0 0 1px rgba(255,255,255,0.10);
    color: rgba(220,230,245,0.85); font: 12px ui-monospace, SFMono-Regular, monospace; }
  #chats .chats-head { display: flex; justify-content: space-between; align-items: center; padding: 0 10px 6px 14px; color: rgba(220,230,245,0.55); }
  #chats .chats-toggle { background: transparent; border: 0; color: inherit; cursor: pointer; font: inherit; }
  #chats.collapsed { width: auto; } #chats.collapsed .chats-list, #chats.collapsed .chats-new { display: none; }
  #chats.collapsed .chats-head span { display: none; }
  #chats ul { list-style: none; margin: 0; padding: 0; max-height: 50vh; overflow-y: auto; }
  #chats li { display: flex; gap: 8px; padding: 4px 14px; cursor: pointer; }
  #chats li:hover { background: rgba(183,140,255,0.16); }
  #chats li .nm { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  #chats li.kept .dot { color: #7ee2a8; } #chats li.main .dot { color: #ffd36b; } #chats li.eph { opacity: 0.7; }
  #chats li .ct { color: rgba(220,230,245,0.45); }
  #chats .chats-new { padding: 6px 14px 2px; color: #b78cff; cursor: pointer; }
  #chats .chats-list { max-height: 28vh; }
  #chats .act-list { max-height: 18vh; }
  #chats .act-n { color: #ffd166; }
  #chats li.ask .dot { color: #ffd166; }
  #chats.collapsed .act-list { display: none; }
  #chats .agents-tree li.agent { cursor: pointer; }
  #chats .agents-head { margin-top: 10px; padding-top: 8px; border-top: 1px solid rgba(255,255,255,0.08); }
  #chats.collapsed .agents-tree, #chats.collapsed .agents-head { display: none; }
  #chats .agents-tree { max-height: 46vh; }
  #chats .agents-tree li { gap: 6px; padding: 2px 10px; cursor: default; }
  #chats .agents-tree li.kernel { color: rgba(220,230,245,0.55); padding: 6px 10px 2px; }
  #chats .agents-tree li .tw { width: 10px; cursor: pointer; color: rgba(220,230,245,0.5); }
  #chats .agents-tree li .gl { width: 12px; color: rgba(220,230,245,0.5); }
  #chats .agents-tree li.agent { opacity: 0.6; }
  #chats .agents-tree li.has-cell { opacity: 1; cursor: pointer; }
  #chats .agents-tree li.has-cell .gl { color: #b78cff; }
  #chats .agents-tree li.kernel:hover, #chats .agents-tree li.agent:not(.has-cell):hover { background: transparent; }
  .agent-frame.flash { box-shadow: 0 0 0 2px #b78cff, 0 0 30px rgba(183,140,255,0.6) !important; }
  .canvas-toast { position: fixed; bottom: 18px; left: 50%; transform: translateX(-50%); z-index: 1100;
    background: rgba(40, 20, 26, 0.9); color: #ffb3b3; padding: 8px 14px; border-radius: 10px;
    font: 13px ui-monospace, SFMono-Regular, monospace; box-shadow: 0 0 0 1px rgba(255,120,120,0.3); }
  #toolbar .setup-link { margin-left: 12px; color: #b78cff; text-decoration: none; }
  /* THE BREADCRUMB: root ▸ … ▸ the viewed agent */
  #toolbar .crumbs .crumb { color: rgba(220,230,245,0.7); text-decoration: none; }
  #toolbar .crumbs .crumb:hover { color: #fff; }
  #toolbar .crumbs .crumb.here { color: #fff; font-weight: 600; }
  #toolbar .crumbs .sep { opacity: .45; }
  /* VIEWING ONE AGENT FULL: no world transform, the frame fills the page
     beside the sidebar */
  #canvas.full-view .canvas-world { transform: none !important; }
  #canvas.full-view .agent-frame.full { left: 248px !important; top: 56px !important;
    width: calc(100vw - 260px) !important; height: calc(100vh - 68px) !important; }
  #canvas.full-view .agent-frame.full .resize-handle { display: none; }
  #chats .agent.viewed { background: rgba(183,140,255,0.18); }
  #toolbar .setup-link:hover { text-decoration: underline; }
  #toolbar code { color: #cde; background: rgba(255,255,255,0.08); padding: 1px 6px; border-radius: 4px; }
`;
