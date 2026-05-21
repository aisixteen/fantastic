// telemetry_pane GL view — live agent visualization.
//
// Runs as the body of:
//     new Function('THREE', 'scene', 't', 'onFrame', 'cleanup', source)
//
// Each agent in the substrate is a Three.js Sprite with a CanvasTexture
// redrawn on state events + per-frame pulse decay. Each `send`/`emit`
// stacks a `glow` value (capped at ~2.2) which decays exponentially
// (~88%/frame ≈ 0.2s τ). Rapid bursts pile a bit but don't oversaturate.
//
// Visual: dim gray border idle → on traffic, bright white inner core +
// neon Tron halo (cyan for `send`, mint for `emit`) + subtle white BG
// tint at peak intensity.
//
// Connection rays: every `send` event with a real-agent `sender` draws
// a fading neon line from sender's sprite → recipient's sprite. Same
// glow-stacking + per-frame decay. Self-sends are skipped.
//
// Layout: insertion-order slot allocation, 4 cols × N rows. An agent
// keeps its slot for its lifetime; on remove the slot is freed and
// reclaimed.
//
// Pure consumer of the kernel state stream. No `t.call` / `t.send` /
// `t.emit` from inside the render path — visualizing yourself does
// NOT feedback-loop.

// Three layers, all behind the camera plane (depthTest off everywhere):
//   raysGroup    — static wires between sender/receiver, painted first
//   pulsesGroup  — fast traveling glow ping along each wire
//   agentGroup   — sprites on top
// Three.js sorts by per-object renderOrder; lower draws first.
const raysGroup = new THREE.Group();
scene.add(raysGroup);
const pulsesGroup = new THREE.Group();
scene.add(pulsesGroup);
const agentGroup = new THREE.Group();
scene.add(agentGroup);

const agentSprites = new Map();
const rays = [];
const pulses = [];

const GRID_COLS = 4;
const COL_PITCH = 32;
const ROW_PITCH = 20;
const X_OFFSET = -((GRID_COLS - 1) * COL_PITCH) / 2;
const Y_TOP = 28;
const slotFreeList = [];
let nextNewSlot = 0;

// Pulse / ray dynamics. Adjust here to tune feel.
const GLOW_INCREMENT = 0.65;   // added per traffic event
const GLOW_CAP = 2.2;          // burst ceiling so rapid bursts don't oversaturate
const GLOW_DECAY = 0.88;       // per-frame multiplier (~0.2s τ at 60fps)
const RAY_GLOW_INITIAL = 1.0;
const RAY_GLOW_DECAY = 0.88;
const RAY_GLOW_FLOOR = 0.02;
const RAYS_MAX = 60;           // cap to prevent runaway count
const PULSE_DURATION = 0.18;   // seconds to traverse the wire (snappy)
const PULSES_MAX = 80;
// Slow water-wobble: each sprite drifts a tiny independent sin/cos
// orbit around its slot. Amplitudes < sprite half-extent so the grid
// reads as still — just barely floating. Frequencies + phases are
// random per sprite so they never sync into a wave.
const WOBBLE_AMP_X = 0.55;
const WOBBLE_AMP_Y = 0.40;
const WOBBLE_FREQ_BASE = 0.18;   // Hz; period ~5.5s
const MESSAGE_LOG_MAX = 10;
const MSG_TRIM_LEN = 80;         // chars per row before "…"

function assignSlot() {
  if (slotFreeList.length > 0) {
    slotFreeList.sort((a, b) => a - b);
    return slotFreeList.shift();
  }
  return nextNewSlot++;
}

function slotPosition(slot) {
  const col = slot % GRID_COLS;
  const row = Math.floor(slot / GRID_COLS);
  return [X_OFFSET + col * COL_PITCH, Y_TOP - row * ROW_PITCH];
}

function redrawAgentTexture(entry, state) {
  const { ctx, canvas, texture } = entry;
  const W = canvas.width, H = canvas.height;
  const glow = state.glow || 0;
  const intensity = Math.min(glow, 1);   // 0..1 for color/alpha curves
  ctx.clearRect(0, 0, W, H);

  // Background panel — slight white tint at peak intensity.
  ctx.fillStyle = `rgba(14, 14, 22, ${0.78 - intensity * 0.05})`;
  ctx.fillRect(0, 0, W, H);
  if (intensity > 0) {
    ctx.fillStyle = `rgba(255, 255, 255, ${intensity * 0.10})`;
    ctx.fillRect(0, 0, W, H);
  }

  // Border — dim gray idle; Tron neon ring stack on traffic.
  if (intensity > 0) {
    const tint = state.lastBlipKind === 'emit'
      ? [180, 255, 220]   // mint
      : [180, 220, 255];  // cyan
    // Outer halo: a few decreasing-alpha strokes for the glow.
    for (let layer = 2; layer >= 0; layer--) {
      const alpha = intensity * (0.20 + layer * 0.18);
      const w = 1 + layer * 2 + intensity * 1.5;
      ctx.strokeStyle = `rgba(${tint[0]}, ${tint[1]}, ${tint[2]}, ${alpha})`;
      ctx.lineWidth = w;
      ctx.strokeRect(layer + 1, layer + 1, W - 2 * (layer + 1), H - 2 * (layer + 1));
    }
    // Inner bright white core line.
    ctx.strokeStyle = `rgba(255, 255, 255, ${intensity * 0.95})`;
    ctx.lineWidth = 1.5;
    ctx.strokeRect(2, 2, W - 4, H - 4);
  } else {
    ctx.strokeStyle = 'rgba(60, 60, 86, 1)';
    ctx.lineWidth = 1;
    ctx.strokeRect(1, 1, W - 2, H - 2);
  }

  // Name (truncated to fit).
  ctx.fillStyle = '#cdedff';
  ctx.font = '600 18px ui-monospace, SFMono-Regular, monospace';
  const name = state.name || state.agent_id;
  const maxNameW = W - 24;
  let drawName = name;
  while (ctx.measureText(drawName).width > maxNameW && drawName.length > 4) {
    drawName = drawName.slice(0, -2);
  }
  if (drawName !== name) drawName = drawName.slice(0, -1) + '…';
  ctx.fillText(drawName, 12, 30);

  // 10 backlog dots in a 5x2 grid.
  const dotR = 6, dotPitch = 22;
  for (let i = 0; i < 10; i++) {
    const col = i % 5, row = Math.floor(i / 5);
    ctx.beginPath();
    ctx.arc(22 + col * dotPitch, 64 + row * dotPitch, dotR, 0, Math.PI * 2);
    ctx.fillStyle = (state.backlog > i) ? '#ffd166' : '#22222a';
    ctx.fill();
  }
  // +N more overflow.
  if (state.backlog > 10) {
    ctx.fillStyle = '#ffd166';
    ctx.font = '14px ui-monospace, SFMono-Regular, monospace';
    ctx.fillText('+' + (state.backlog - 10) + ' more', 140, 78);
  }
  texture.needsUpdate = true;
}

function ensureAgentSprite(agent_id, name, backlog) {
  let entry = agentSprites.get(agent_id);
  if (entry) {
    const newState = Object.assign({}, entry.lastState, {
      name: name || entry.lastState.name,
      backlog: typeof backlog === 'number' ? backlog : entry.lastState.backlog,
    });
    entry.lastState = newState;
    redrawAgentTexture(entry, newState);
    return entry;
  }
  const canvas = document.createElement('canvas');
  canvas.width = 256; canvas.height = 144;
  const ctx = canvas.getContext('2d');
  const texture = new THREE.CanvasTexture(canvas);
  texture.minFilter = THREE.LinearFilter;
  texture.generateMipmaps = false;
  const material = new THREE.SpriteMaterial({
    map: texture, transparent: true, depthTest: false, depthWrite: false,
  });
  const sprite = new THREE.Sprite(material);
  sprite.renderOrder = 2;   // sprites paint on top of rays + pulses
  sprite.scale.set(28, 16, 1);
  const slot = assignSlot();
  const [x, y] = slotPosition(slot);
  sprite.position.set(x, y, 0);
  agentGroup.add(sprite);
  entry = {
    sprite, texture, ctx, canvas, agent_id, slot,
    baseX: x, baseY: y,
    // Independent wobble per sprite — random phase + slight per-axis
    // frequency variation so the grid never beats in unison.
    wobble: {
      px: Math.random() * Math.PI * 2,
      py: Math.random() * Math.PI * 2,
      fx: WOBBLE_FREQ_BASE * (0.7 + Math.random() * 0.6),
      fy: WOBBLE_FREQ_BASE * (0.7 + Math.random() * 0.6),
    },
    lastState: {
      agent_id, name: name || agent_id, backlog: backlog || 0,
      glow: 0, lastBlipKind: null,
    },
  };
  agentSprites.set(agent_id, entry);
  redrawAgentTexture(entry, entry.lastState);
  return entry;
}

function removeAgentSprite(agent_id) {
  const entry = agentSprites.get(agent_id);
  if (!entry) return;
  agentGroup.remove(entry.sprite);
  entry.texture.dispose();
  entry.sprite.material.dispose();
  agentSprites.delete(agent_id);
  slotFreeList.push(entry.slot);
}

function triggerBlip(agent_id, kind) {
  const entry = agentSprites.get(agent_id);
  if (!entry) return;
  const s = entry.lastState;
  s.glow = Math.min((s.glow || 0) + GLOW_INCREMENT, GLOW_CAP);
  s.lastBlipKind = kind;
  // Per-frame tick will redraw on the next rAF.
}

// ─── messages pane (last N) ─────────────────────────────────────
// Vertical sprite to the right of the agent grid. Each `send`/`emit`
// state event pushes one row; the pane shows up to MESSAGE_LOG_MAX
// (newest first) with sender → target tinted by kind, plus a trimmed
// payload summary on the second line.
const messageLog = [];
const msgCanvas = document.createElement('canvas');
msgCanvas.width = 384;
msgCanvas.height = 720;
const msgCtx = msgCanvas.getContext('2d');
const msgTexture = new THREE.CanvasTexture(msgCanvas);
msgTexture.minFilter = THREE.LinearFilter;
msgTexture.generateMipmaps = false;
const msgMaterial = new THREE.SpriteMaterial({
  map: msgTexture, transparent: true, depthTest: false, depthWrite: false,
});
const msgSprite = new THREE.Sprite(msgMaterial);
msgSprite.renderOrder = 2;
msgSprite.scale.set(40, 75, 1);   // tall narrow column
// Park to the right of the rightmost grid column. Grid right edge is
// X_OFFSET + (GRID_COLS-1)*COL_PITCH + sprite_half_width (~14).
msgSprite.position.set(
  X_OFFSET + (GRID_COLS - 1) * COL_PITCH + 14 + 10 + 20,
  Y_TOP - 30, 0,
);
agentGroup.add(msgSprite);

function _trimText(ctx, text, maxW) {
  if (!text) return '';
  if (ctx.measureText(text).width <= maxW) return text;
  let s = text;
  while (s.length > 4 && ctx.measureText(s + '…').width > maxW) {
    s = s.slice(0, -1);
  }
  return s + '…';
}

function redrawMessagePane() {
  const W = msgCanvas.width, H = msgCanvas.height;
  const ctx = msgCtx;
  ctx.clearRect(0, 0, W, H);
  ctx.fillStyle = 'rgba(14, 14, 22, 0.82)';
  ctx.fillRect(0, 0, W, H);
  ctx.strokeStyle = 'rgba(60, 60, 86, 1)';
  ctx.lineWidth = 1;
  ctx.strokeRect(1, 1, W - 2, H - 2);
  // Header.
  ctx.fillStyle = '#cdedff';
  ctx.font = '600 22px ui-monospace, SFMono-Regular, monospace';
  ctx.fillText('messages', 16, 34);
  ctx.strokeStyle = 'rgba(60, 60, 86, 1)';
  ctx.beginPath();
  ctx.moveTo(0, 50); ctx.lineTo(W, 50); ctx.stroke();

  const ROW_H = 62;
  const startY = 60;
  for (let i = 0; i < messageLog.length; i++) {
    const m = messageLog[i];
    const y = startY + i * ROW_H;
    if (y + ROW_H > H) break;
    const tint = m.kind === 'emit' ? '#b4ffdc' : '#b4dcff';
    ctx.fillStyle = tint;
    ctx.font = '600 14px ui-monospace, SFMono-Regular, monospace';
    const head = `${m.sender || '∅'} → ${m.target}  [${m.kind}]`;
    ctx.fillText(_trimText(ctx, head, W - 24), 14, y + 16);
    ctx.fillStyle = '#9ab';
    ctx.font = '13px ui-monospace, SFMono-Regular, monospace';
    let body = m.summary || '';
    if (body.length > MSG_TRIM_LEN) body = body.slice(0, MSG_TRIM_LEN - 1) + '…';
    ctx.fillText(_trimText(ctx, body, W - 24), 14, y + 36);
    ctx.strokeStyle = 'rgba(60, 60, 86, 0.5)';
    ctx.beginPath();
    ctx.moveTo(8, y + 50); ctx.lineTo(W - 8, y + 50); ctx.stroke();
  }
  msgTexture.needsUpdate = true;
}

function pushMessage(sender, target, kind, summary) {
  messageLog.unshift({ sender, target, kind, summary });
  if (messageLog.length > MESSAGE_LOG_MAX) messageLog.length = MESSAGE_LOG_MAX;
  redrawMessagePane();
}

redrawMessagePane();

// Stacked layers per ray: wide-dim outer halo → narrow-bright core.
// `LineBasicMaterial.linewidth` is silently ignored on most WebGL
// drivers (always 1px), so we build the ray out of oriented
// PlaneGeometry quads instead. Each layer is a separate material so
// per-frame glow decay can fade them independently.
const RAY_LAYERS = [
  { w: 4.5, a: 0.08 },   // outermost halo
  { w: 2.6, a: 0.20 },
  { w: 1.4, a: 0.45 },
  { w: 0.6, a: 0.95 },   // bright core
];

function addRay(fromId, toId, kind) {
  const a = agentSprites.get(fromId);
  const b = agentSprites.get(toId);
  if (!a || !b || a === b) return;   // skip self-sends + missing peers
  const sx = a.sprite.position.x, sy = a.sprite.position.y;
  const ex = b.sprite.position.x, ey = b.sprite.position.y;
  const dx = ex - sx, dy = ey - sy;
  const length = Math.hypot(dx, dy);
  if (length < 1e-3) return;
  const color = kind === 'emit' ? 0xb4ffdc : 0xb4dcff;   // mint / cyan
  const group = new THREE.Group();
  group.position.set((sx + ex) / 2, (sy + ey) / 2, 0);
  group.rotation.z = Math.atan2(dy, dx);
  const layers = [];
  for (const L of RAY_LAYERS) {
    const geom = new THREE.PlaneGeometry(length, L.w);
    const mat = new THREE.MeshBasicMaterial({
      color, transparent: true, opacity: L.a,
      depthTest: false, depthWrite: false,
    });
    const mesh = new THREE.Mesh(geom, mat);
    mesh.renderOrder = 0;   // rays render BEHIND sprites (wires under cards)
    group.add(mesh);
    layers.push({ mat, geom, baseAlpha: L.a });
  }
  raysGroup.add(group);
  rays.push({ group, layers, glow: RAY_GLOW_INITIAL, kind });
  // Cap to prevent unbounded growth on storms of traffic.
  while (rays.length > RAYS_MAX) {
    const old = rays.shift();
    raysGroup.remove(old.group);
    for (const L of old.layers) { L.geom.dispose(); L.mat.dispose(); }
  }
  // A pulse runs from sender → receiver along this ray for that
  // extra "data packet zooming down the wire" feel.
  addPulse(sx, sy, ex, ey, color);
}

// Layered glow: same color as the ray, brighter at the core, soft
// halo around it. Each pulse is a tiny multi-mesh group lerped from
// start to end over PULSE_DURATION seconds.
const PULSE_LAYERS = [
  { size: 6.5, alpha: 0.10 },
  { size: 4.0, alpha: 0.25 },
  { size: 2.2, alpha: 0.65 },
  { size: 1.0, alpha: 1.00 },   // bright core
];

function addPulse(sx, sy, ex, ey, color) {
  const group = new THREE.Group();
  const layers = [];
  for (const L of PULSE_LAYERS) {
    const geom = new THREE.PlaneGeometry(L.size, L.size);
    const mat = new THREE.MeshBasicMaterial({
      color, transparent: true, opacity: L.alpha,
      depthTest: false, depthWrite: false,
    });
    const mesh = new THREE.Mesh(geom, mat);
    mesh.renderOrder = 1;   // pulses sit ABOVE rays, BELOW sprites
    group.add(mesh);
    layers.push({ mat, geom, baseAlpha: L.alpha });
  }
  group.position.set(sx, sy, 0);
  pulsesGroup.add(group);
  pulses.push({ group, layers, sx, sy, ex, ey, t: 0 });
  while (pulses.length > PULSES_MAX) {
    const old = pulses.shift();
    pulsesGroup.remove(old.group);
    for (const L of old.layers) { L.geom.dispose(); L.mat.dispose(); }
  }
}

// Per-frame pulse + ray decay. Cheap when nothing's animating
// (early-out on glow == 0). `time` is seconds since GL host startup.
let lastFrameTime = 0;
onFrame((time) => {
  const dt = lastFrameTime > 0 ? Math.min(time - lastFrameTime, 0.1) : 1 / 60;
  lastFrameTime = time;
  for (const entry of agentSprites.values()) {
    const s = entry.lastState;
    if (s.glow > 0) {
      s.glow *= GLOW_DECAY;
      if (s.glow < 0.01) s.glow = 0;
      redrawAgentTexture(entry, s);
    }
    // Water-float wobble. Cheap (two sin/cos per sprite per frame).
    const w = entry.wobble;
    entry.sprite.position.x = entry.baseX +
      Math.sin(time * w.fx * 2 * Math.PI + w.px) * WOBBLE_AMP_X;
    entry.sprite.position.y = entry.baseY +
      Math.cos(time * w.fy * 2 * Math.PI + w.py) * WOBBLE_AMP_Y;
  }
  for (let i = rays.length - 1; i >= 0; i--) {
    const r = rays[i];
    r.glow *= RAY_GLOW_DECAY;
    if (r.glow < RAY_GLOW_FLOOR) {
      raysGroup.remove(r.group);
      for (const L of r.layers) { L.geom.dispose(); L.mat.dispose(); }
      rays.splice(i, 1);
    } else {
      // Each layer fades from its own base alpha — keeps the halo
      // gradient intact while the whole ray dims.
      for (const L of r.layers) L.mat.opacity = L.baseAlpha * r.glow;
    }
  }
  // Pulses traveling along their wires. Each runs t in [0,1] over
  // PULSE_DURATION seconds (snappy ~0.18s); also fade the trailing
  // half so the head feels brighter than the tail.
  for (let i = pulses.length - 1; i >= 0; i--) {
    const p = pulses[i];
    p.t += dt / PULSE_DURATION;
    if (p.t >= 1) {
      pulsesGroup.remove(p.group);
      for (const L of p.layers) { L.geom.dispose(); L.mat.dispose(); }
      pulses.splice(i, 1);
      continue;
    }
    p.group.position.x = p.sx + (p.ex - p.sx) * p.t;
    p.group.position.y = p.sy + (p.ey - p.sy) * p.t;
    // Bright most of the run, fade in the last 30% so the impact
    // softens into the receiver instead of popping.
    const fade = p.t < 0.7 ? 1.0 : 1.0 - (p.t - 0.7) / 0.3;
    for (const L of p.layers) L.mat.opacity = L.baseAlpha * fade;
  }
});

const unsubState = t.subscribeState((evt) => {
  if (evt.type === 'state_snapshot') {
    for (const a of evt.agents) {
      ensureAgentSprite(a.agent_id, a.name, a.backlog);
    }
  } else if (evt.type === 'state_event') {
    if (evt.kind === 'added') {
      ensureAgentSprite(evt.agent_id, evt.name, 0);
    } else if (evt.kind === 'removed') {
      removeAgentSprite(evt.agent_id);
    } else if (evt.kind === 'updated') {
      const cur = agentSprites.get(evt.agent_id);
      ensureAgentSprite(
        evt.agent_id, evt.name,
        cur ? cur.lastState.backlog : 0
      );
    } else if (evt.kind === 'drain') {
      ensureAgentSprite(evt.agent_id, evt.name, evt.backlog);
    } else {
      // 'send' or 'emit' — traffic event.
      ensureAgentSprite(evt.agent_id, evt.name, evt.backlog);
      triggerBlip(evt.agent_id, evt.kind);
      pushMessage(evt.sender, evt.agent_id, evt.kind, evt.summary || '');
      // Fading ray from sender → recipient when sender is a real
      // agent we know about. External entry points (proxy / cli)
      // leave sender=null and produce no ray.
      if (evt.sender) {
        addRay(evt.sender, evt.agent_id, evt.kind);
      }
    }
  }
});

cleanup.push(() => {
  unsubState();
  for (const entry of agentSprites.values()) {
    agentGroup.remove(entry.sprite);
    entry.texture.dispose();
    entry.sprite.material.dispose();
  }
  agentSprites.clear();
  for (const r of rays) {
    raysGroup.remove(r.group);
    for (const L of r.layers) { L.geom.dispose(); L.mat.dispose(); }
  }
  rays.length = 0;
  for (const p of pulses) {
    pulsesGroup.remove(p.group);
    for (const L of p.layers) { L.geom.dispose(); L.mat.dispose(); }
  }
  pulses.length = 0;
  agentGroup.remove(msgSprite);
  msgTexture.dispose();
  msgMaterial.dispose();
  scene.remove(agentGroup);
  scene.remove(raysGroup);
  scene.remove(pulsesGroup);
});
