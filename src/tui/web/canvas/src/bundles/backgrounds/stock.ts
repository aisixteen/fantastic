// THE STOCK BACKGROUNDS (owner 2026-10-09) — the owner's set: Isoline, Glyph and
// Copper from the bganim canvas's panel, and Moire (made there by an AI). From that
// backgrounds panel: one shared PRELUDE (disposables, the clock, a baked noise
// texture, GLSL helpers, a world backdrop plane, the headline) and, per
// background, a BODY and its colour SETS (4 hex: field, line, mid, accent).
// A source is `const SET = <colours>; ` + PRELUDE + body (`sourceOf`). DATA —
// regenerate it rather than edit it by hand.

export interface StockBackground {
  name: string;
  note: string;
  sets: Array<[string, string[]]>;
  body: string;
}

export const PRELUDE: string = "// ── shared prelude for every background: disposables, the clock, the colour set, a baked\n// noise texture, GLSL helpers, a world backdrop plane, and the headline pinned top-left.\n// `SET` (4 hex colours: field, line, mid, accent) is defined just before this prelude.\nconst disposables = [];\nconst keep = (...o) => { disposables.push(...o); return o[0]; };\nconst PR = Math.min(window.devicePixelRatio || 1, 2);\nconst rgb = h => { const n = parseInt(h.slice(1), 16); return new THREE.Vector3((n >> 16 & 255) / 255, (n >> 8 & 255) / 255, (n & 255) / 255); };\n\n// fbm noise baked ONCE per page into a tileable 256² texture (R and G: two seeds), so a\n// shader reads noise with one texture fetch instead of 16 hashes per fbm call\nfunction bakeNoise() {\n  if (globalThis.__fantasticNoise) return globalThis.__fantasticNoise;\n  const N = 256, data = new Uint16Array(N * N * 4);\n  const chan = seed => {\n    const out = new Float32Array(N * N);\n    let amp = 0.5;\n    for (let o = 0; o < 4; o++, amp *= 0.5) {\n      const P = 8 << o, g = new Float32Array(P * P);\n      let s = seed * 9301 + o * 49297 + 1;\n      for (let i = 0; i < P * P; i++) { s = (s * 16807) % 2147483647; g[i] = s / 2147483647; }\n      for (let y = 0; y < N; y++) {\n        const fy = y * P / N, y0 = Math.floor(fy), ty = fy - y0, sy = ty * ty * (3 - 2 * ty), y1 = (y0 + 1) % P;\n        for (let x = 0; x < N; x++) {\n          const fx = x * P / N, x0 = Math.floor(fx), tx = fx - x0, sx = tx * tx * (3 - 2 * tx), x1 = (x0 + 1) % P;\n          const top = g[y0 * P + x0] + (g[y0 * P + x1] - g[y0 * P + x0]) * sx;\n          const bot = g[y1 * P + x0] + (g[y1 * P + x1] - g[y1 * P + x0]) * sx;\n          out[y * N + x] += amp * (top + (bot - top) * sy);\n        }\n      }\n    }\n    return out;\n  };\n  const r = chan(1), gch = chan(2);\n  for (let i = 0; i < N * N; i++) {\n    data[i * 4] = THREE.DataUtils.toHalfFloat(r[i]);\n    data[i * 4 + 1] = THREE.DataUtils.toHalfFloat(gch[i]);\n    data[i * 4 + 3] = THREE.DataUtils.toHalfFloat(1);\n  }\n  return (globalThis.__fantasticNoise = { data, N });\n}\nconst nz = bakeNoise();\nconst noiseTex = keep(new THREE.DataTexture(nz.data, nz.N, nz.N, THREE.RGBAFormat, THREE.HalfFloatType));\nnoiseTex.wrapS = noiseTex.wrapT = THREE.RepeatWrapping;\nnoiseTex.minFilter = noiseTex.magFilter = THREE.LinearFilter;\nnoiseTex.generateMipmaps = false;\nnoiseTex.needsUpdate = true;\n\nconst uniforms = {\n  uT: { value: 0 }, uPR: { value: PR }, uNoise: { value: noiseTex },\n  uC0: { value: rgb(SET[0]) }, uC1: { value: rgb(SET[1]) }, uC2: { value: rgb(SET[2]) }, uC3: { value: rgb(SET[3]) },\n};\nonFrame(time => { uniforms.uT.value = time; });\nconst done = () => disposables.forEach(d => d.dispose());\nif (typeof cleanup === 'function') cleanup(done); else cleanup.push(done);\n\n// fbm(p) / fbm2(p): one texture fetch (the tile spans 8 noise units, like the old lattice)\nconst GLSL = `\n  uniform sampler2D uNoise; uniform vec3 uC0, uC1, uC2, uC3;\n  float hash(vec2 p){ return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453); }\n  float fbm(vec2 p){ return texture2D(uNoise, p * 0.125).r; }\n  vec2 fbm2(vec2 p){ return texture2D(uNoise, p * 0.125).rg; }\n  float grain(float t){ return hash(gl_FragCoord.xy + fract(t * 7.0) * 91.0) - 0.5; }\n  float hair(float v, float w){ float d = abs(fract(v + 0.5) - 0.5) / max(fwidth(v), 1e-4); return 1.0 - smoothstep(0.0, w, d); }\n  float hairAt(float v, float at, float w){ float d = abs(v - at) / max(fwidth(v), 1e-4); return 1.0 - smoothstep(0.0, w, d); }\n  float vignette(vec2 w){ return 0.55 + 0.45 * smoothstep(1.7, 0.3, length(w / vec2(3400.0, 2000.0))); }\n`;\n\n// a huge plane far back; `body` is the fragment main() and sees world xy as vW.xy\nfunction backdrop(body, opt = {}) {\n  const geo = keep(new THREE.PlaneGeometry(80000, 40000));\n  const mat = keep(new THREE.ShaderMaterial({\n    uniforms: { ...uniforms, ...(opt.uniforms || {}) }, depthWrite: false,\n    vertexShader: `varying vec3 vW; void main(){ vec4 w = modelMatrix * vec4(position, 1.0); vW = w.xyz; gl_Position = projectionMatrix * viewMatrix * w; }`,\n    fragmentShader: `uniform float uT, uPR; varying vec3 vW; ${opt.decl || ''} ${GLSL} void main(){ ${body} }`,\n  }));\n  const mesh = new THREE.Mesh(geo, mat);\n  mesh.position.z = opt.z ?? -900;\n  mesh.renderOrder = -10;\n  mesh.frustumCulled = false;\n  scene.add(mesh);\n  return mesh;\n}\n\n// lowercase light system-sans headline, a short accent bar, a mono tagline — the canvas\n// UI's own fonts — pinned top-left with padding at a fixed pixel size, whatever the zoom\nfunction headline({ ink = '#ededeb', sub = '#8a8884', accent = SET[3] } = {}) {\n  const HEAD = 'fantastic canvas', SUB = 'Ultimate creativity is you';\n  const SCALE = 0.3, PAD_PX = 36, M = 24, D = 30, SUB_GAP = 92, BAR_W = 120, BAR_H = 8;\n  const headFont = `300 230px system-ui, sans-serif`;\n  const subFont = `400 50px ui-monospace, SFMono-Regular, Menlo, monospace`;\n  const c = document.createElement('canvas');\n  const ctx0 = c.getContext('2d');\n  ctx0.font = headFont;\n  if ('letterSpacing' in ctx0) ctx0.letterSpacing = '-6px';\n  const m = ctx0.measureText(HEAD);\n  ctx0.font = subFont;\n  if ('letterSpacing' in ctx0) ctx0.letterSpacing = '1px';\n  const ms = ctx0.measureText(SUB);\n  const tw = Math.ceil(Math.max(m.width, ms.width));\n  const th = Math.ceil(m.actualBoundingBoxAscent + m.actualBoundingBoxDescent + SUB_GAP + ms.actualBoundingBoxAscent + ms.actualBoundingBoxDescent);\n  c.width = tw + M * 2; c.height = th + M * 2;\n  const ctx = c.getContext('2d');\n  ctx.textAlign = 'left'; ctx.textBaseline = 'alphabetic';\n  ctx.font = headFont;\n  if ('letterSpacing' in ctx) ctx.letterSpacing = '-6px';\n  const tx = M + m.actualBoundingBoxLeft, ty = M + m.actualBoundingBoxAscent;\n  ctx.fillStyle = ink;\n  ctx.fillText(HEAD, tx, ty);\n  ctx.fillStyle = accent;\n  ctx.fillRect(tx + 6, ty + m.actualBoundingBoxDescent + SUB_GAP / 2 - BAR_H / 2, BAR_W, BAR_H);\n  ctx.font = subFont;\n  if ('letterSpacing' in ctx) ctx.letterSpacing = '1px';\n  ctx.fillStyle = sub;\n  ctx.fillText(SUB, M + ms.actualBoundingBoxLeft + 4, ty + m.actualBoundingBoxDescent + SUB_GAP + ms.actualBoundingBoxAscent);\n  const TITLE_PX = c.width * SCALE;\n  const tex = keep(new THREE.CanvasTexture(c));\n  tex.anisotropy = 8;\n  const aspect = c.height / c.width;\n  const geo = keep(new THREE.PlaneGeometry(1, aspect));\n  geo.translate(0.5, -0.5 * aspect, 0); // origin at the plane's top-left\n  const mat = keep(new THREE.MeshBasicMaterial({ map: tex, transparent: true, depthWrite: false, depthTest: false }));\n  const title = new THREE.Mesh(geo, mat);\n  title.frustumCulled = false;\n  title.renderOrder = 10;\n  title.matrixAutoUpdate = false;\n  const vp = new THREE.Vector2(), off = new THREE.Matrix4(), sc = new THREE.Matrix4();\n  title.onBeforeRender = (renderer, _s, camera) => {\n    renderer.getSize(vp);\n    const halfH = D * Math.tan((camera.fov * Math.PI) / 360) / camera.zoom;\n    const wpp = (2 * halfH) / vp.y;                  // world units per css pixel at depth D\n    const margin = (M / c.width) * TITLE_PX;         // the texture margin, in pixels on screen\n    off.makeTranslation(-halfH * camera.aspect + (PAD_PX - margin) * wpp, halfH - (PAD_PX - margin) * wpp, -D);\n    sc.makeScale(TITLE_PX * wpp, TITLE_PX * wpp, 1);\n    title.matrixWorld.multiplyMatrices(camera.matrixWorld, off).multiply(sc);\n  };\n  scene.add(title);\n}\n\n";

export const STOCK: readonly StockBackground[] = [
 {
  "name": "Isoline",
  "note": "topographic hairlines · one burning contour",
  "sets": [
   [
    "Ember",
    [
     "#050506",
     "#8a8884",
     "#6b1d08",
     "#ff6a1f"
    ]
   ],
   [
    "Lagoon",
    [
     "#040607",
     "#7f8b8a",
     "#0c3a36",
     "#2fd0bd"
    ]
   ],
   [
    "Damson",
    [
     "#060407",
     "#8d7f8f",
     "#3d1546",
     "#ff4d8d"
    ]
   ],
   [
    "Cloud",
    [
     "#060606",
     "#8a8a88",
     "#2e2e2c",
     "#f0eee9"
    ]
   ]
  ],
  "body": "// Isoline — a slow topographic field of hairline contours; one contour burns in the accent\nbackdrop(`\n  float t = uT * 0.012; vec2 p = vW.xy / 1100.0;\n  float n = fbm(p + vec2(t, -t * 0.6) + 0.35 * fbm2(p * 0.7 + vec2(-t, t)));\n  float v = n * 16.0;\n  vec3 c = uC0 + uC1 * (0.19 * hair(v, 1.0) + 0.13 * hair(v / 4.0, 1.2));\n  float lvl = 7.5 + 1.5 * sin(uT * 0.03);\n  c += uC3 * hairAt(v, lvl, 1.6) * 0.9 + uC2 * exp(-abs(v - lvl) * 3.5) * 0.5;\n  c *= vignette(vW.xy);\n  gl_FragColor = vec4(c + grain(uT) * 0.025, 1.0);\n`);\nheadline();\n"
 },
 {
  "name": "Glyph",
  "note": "ASCII density field in the canvas mono",
  "sets": [
   [
    "Ember",
    [
     "#050506",
     "#8a8884",
     "#6b1d08",
     "#ff6a1f"
    ]
   ],
   [
    "Lagoon",
    [
     "#040607",
     "#7f8b8a",
     "#0c3a36",
     "#2fd0bd"
    ]
   ],
   [
    "Damson",
    [
     "#060407",
     "#8d7f8f",
     "#3d1546",
     "#ff4d8d"
    ]
   ],
   [
    "Cloud",
    [
     "#060606",
     "#8a8a88",
     "#2e2e2c",
     "#f0eee9"
    ]
   ]
  ],
  "body": "// Glyph — an ASCII density field in the canvas mono; rare glyphs flicker in the accent\nconst atlas = document.createElement('canvas');\natlas.width = 640; atlas.height = 64;\n{\n  const g = atlas.getContext('2d');\n  g.font = `500 50px ui-monospace, SFMono-Regular, Menlo, monospace`;\n  g.fillStyle = '#fff'; g.textAlign = 'center'; g.textBaseline = 'middle';\n  [...' .:-=+*#%@'].forEach((ch, i) => g.fillText(ch, i * 64 + 32, 34));\n}\nconst atlasTex = keep(new THREE.CanvasTexture(atlas));\natlasTex.minFilter = THREE.LinearFilter; atlasTex.generateMipmaps = false;\nbackdrop(`\n  float S = 18.0;\n  vec2 cell = floor(vW.xy / S), uv = fract(vW.xy / S), ctr = (cell + 0.5) * S;\n  float t = uT * 0.015;\n  float n = fbm(ctr / 1000.0 + vec2(t, -t)) + 0.09 * sin(ctr.y * 0.003 - uT * 0.12);\n  float lvl = clamp((n - 0.26) * 2.0, 0.0, 0.999);\n  float g = texture2D(uAtlas, vec2((floor(lvl * 10.0) + uv.x) / 10.0, uv.y)).a;\n  float hot = max(step(0.985, hash(cell + floor(uT * 0.5))) * step(0.3, lvl), smoothstep(0.9, 0.99, lvl));\n  vec3 col = mix(uC1 * 0.9, uC3, hot);\n  float px = fwidth(vW.x), vis = 1.0 - smoothstep(S / 10.0, S / 5.0, px);\n  vec3 c = uC0 + col * mix(lvl * 0.25, g, vis) * (0.3 + 0.7 * lvl) * 0.85;\n  c *= vignette(vW.xy);\n  gl_FragColor = vec4(c, 1.0);\n`, { decl: `uniform sampler2D uAtlas;`, uniforms: { uAtlas: { value: atlasTex } } });\nheadline();\n"
 },
 {
  "name": "Copper",
  "note": "wire terrain · lit crests",
  "sets": [
   [
    "Ember",
    [
     "#050506",
     "#8a8884",
     "#6b1d08",
     "#ff6a1f"
    ]
   ],
   [
    "Lagoon",
    [
     "#040607",
     "#7f8b8a",
     "#0c3a36",
     "#2fd0bd"
    ]
   ],
   [
    "Damson",
    [
     "#060407",
     "#8d7f8f",
     "#3d1546",
     "#ff4d8d"
    ]
   ],
   [
    "Cloud",
    [
     "#060606",
     "#8a8a88",
     "#2e2e2c",
     "#f0eee9"
    ]
   ]
  ],
  "body": "// Copper — a dark wire terrain rolling toward you; only the crests catch the accent light\nbackdrop(`\n  vec3 c = uC0 + uC2 * exp(-pow((vW.y + 120.0) / 260.0, 2.0)) * 0.5;\n  gl_FragColor = vec4(c * vignette(vW.xy) + grain(uT) * 0.02, 1.0);\n`, { z: -1100 });\n{\n  const lp = [], STEP = 36, SEG = 9, X = 1100, Z0 = 100, Z1 = -1000;\n  for (let x = -X; x <= X; x += STEP) for (let z = Z0; z > Z1; z -= SEG) lp.push(x, 0, z, x, 0, z - SEG);\n  for (let z = Z0; z >= Z1; z -= STEP) for (let x = -X; x < X; x += SEG) lp.push(x, 0, z, x + SEG, 0, z);\n  const geo = keep(new THREE.BufferGeometry());\n  geo.setAttribute('position', new THREE.Float32BufferAttribute(lp, 3));\n  const mat = keep(new THREE.ShaderMaterial({\n    uniforms, transparent: true, depthWrite: false, blending: THREE.AdditiveBlending,\n    vertexShader: `uniform float uT; varying float vF, vH;\n      float hgt(vec2 q){ float t = uT * 0.12;\n        return 26.0 * sin(q.x * 0.006 + q.y * 0.004 - t) + 16.0 * sin(-q.x * 0.009 + q.y * 0.011 - t * 1.4) + 7.0 * sin(q.x * 0.021 + q.y * 0.017 - t * 2.1); }\n      void main(){\n        vec3 p = position; float h = hgt(p.xz); p.y += h; vH = h;\n        vF = (1.0 - smoothstep(-200.0, -980.0, p.z)) * smoothstep(100.0, 20.0, p.z) * (1.0 - smoothstep(600.0, 1100.0, abs(p.x)));\n        gl_Position = projectionMatrix * modelViewMatrix * vec4(p, 1.0);\n      }`,\n    fragmentShader: `uniform vec3 uC1, uC3; varying float vF, vH; void main(){\n      float crest = smoothstep(22.0, 40.0, vH);\n      vec3 c = mix(uC1 * 0.75, uC3, crest) * (0.45 + 0.9 * crest);\n      gl_FragColor = vec4(c * vF * 0.6, 1.0); }`,\n  }));\n  const terrain = new THREE.LineSegments(geo, mat);\n  terrain.position.y = -95;\n  terrain.frustumCulled = false;\n  scene.add(terrain);\n}\nheadline();\n"
 },
 {
  "name": "Moire",
  "note": "two drifting ring fields; accent where they cross",
  "sets": [
   [
    "Ember",
    [
     "#050506",
     "#8a8884",
     "#6b1d08",
     "#ff6a1f"
    ]
   ],
   [
    "Lagoon",
    [
     "#040607",
     "#7f8b8a",
     "#0c3a36",
     "#2fd0bd"
    ]
   ],
   [
    "Damson",
    [
     "#060407",
     "#8d7f8f",
     "#3d1546",
     "#ff4d8d"
    ]
   ],
   [
    "Cloud",
    [
     "#060606",
     "#8a8a88",
     "#2e2e2c",
     "#f0eee9"
    ]
   ]
  ],
  "body": "// Moire — two hairline ring fields drift apart and back; accent only where rings cross\nbackdrop(`\n  vec2 a = vec2(-260.0, -120.0) + 60.0 * vec2(cos(uT * 0.05), sin(uT * 0.04));\n  vec2 b = vec2(240.0, 140.0) + 60.0 * vec2(sin(uT * 0.045), cos(uT * 0.055));\n  float ra = length(vW.xy - a) / 22.0, rb = length(vW.xy - b) / 22.0;\n  float lod = 1.0 - smoothstep(0.25, 0.6, fwidth(ra));\n  float fade = (1.0 - smoothstep(900.0, 1800.0, length(vW.xy))) * lod;\n  float la = hair(ra, 1.0), lb = hair(rb, 1.0);\n  float beat = 0.5 + 0.5 * cos((ra - rb) * 6.2832);\n  vec3 c = uC0 + uC1 * 0.14 * max(la, lb) * fade;\n  c += uC3 * la * lb * fade * 0.9;\n  c += uC2 * beat * 0.3 * exp(-length(vW.xy - mix(a, b, 0.5)) / 500.0);\n  gl_FragColor = vec4(c * vignette(vW.xy) + grain(uT) * 0.02, 1.0);\n`);\nheadline();\n"
 }
];

/** A background's GL source in colour set `colors`. */
export function sourceOf(body: string, colors: readonly string[]): string {
  return `const SET = ${JSON.stringify(colors)}; ` + PRELUDE + body;
}
