// Canvas compositor styles — ported from canvas_webapp/index.html's <style>
// block, minus the #bg WebGL layer (the GL host lands in a later spike; the
// first slice is the DOM compositor only). Injected by mountCanvas().

export const CANVAS_CSS = `
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
  #toolbar code { color: #cde; background: rgba(255,255,255,0.08); padding: 1px 6px; border-radius: 4px; }
`;
