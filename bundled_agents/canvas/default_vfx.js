
// "FANTASTIC" — 3D extruded sci-fi text + neon edge glow
// Scaled 3x and pushed behind UI plane (Z = -3000) so it reads as a background

function createFont(data) {
  return {
    generateShapes: function(text, size) {
      size = size || 100;
      var scale = size / data.resolution;
      var line_height = (data.boundingBox.yMax - data.boundingBox.yMin + data.underlineThickness) * scale;
      var shapes = [];
      var offsetX = 0, offsetY = 0;
      var chars = Array.from(text);
      for (var ci = 0; ci < chars.length; ci++) {
        var ch = chars[ci];
        if (ch === '\n') { offsetX = 0; offsetY -= line_height; continue; }
        var glyph = data.glyphs[ch] || data.glyphs['?'];
        if (!glyph) continue;
        if (glyph.o) {
          var path = new THREE.ShapePath();
          var cmds = glyph.o.split(' ');
          for (var i = 0; i < cmds.length;) {
            var a = cmds[i++];
            if (a === 'm') path.moveTo(cmds[i++]*scale+offsetX, cmds[i++]*scale+offsetY);
            else if (a === 'l') path.lineTo(cmds[i++]*scale+offsetX, cmds[i++]*scale+offsetY);
            else if (a === 'q') {
              var qx=cmds[i++]*scale+offsetX, qy=cmds[i++]*scale+offsetY;
              var qx2=cmds[i++]*scale+offsetX, qy2=cmds[i++]*scale+offsetY;
              path.quadraticCurveTo(qx, qy, qx2, qy2);
            } else if (a === 'b') {
              var bx=cmds[i++]*scale+offsetX, by=cmds[i++]*scale+offsetY;
              var bx2=cmds[i++]*scale+offsetX, by2=cmds[i++]*scale+offsetY;
              var bx3=cmds[i++]*scale+offsetX, by3=cmds[i++]*scale+offsetY;
              path.bezierCurveTo(bx, by, bx2, by2, bx3, by3);
            }
          }
          var s = path.toShapes();
          for (var si = 0; si < s.length; si++) shapes.push(s[si]);
        }
        offsetX += glyph.ha * scale;
      }
      return shapes;
    }
  };
}

var S = 3;  // scale factor
var BG_Z = -3000;  // push behind UI plane (negative Z = farther from camera)
var FONT_URL = 'https://cdn.jsdelivr.net/npm/@compai/font-orbitron@0.0.3/data/typefaces/normal-700.json';
var objects = [];

fetch(FONT_URL)
  .then(function(r) { return r.json(); })
  .then(function(fontData) {
    var font = createFont(fontData);
    var shapes = font.generateShapes('FANTASTIC', 120 * S);

    // ── 3D extruded text body ──
    var geo = new THREE.ExtrudeGeometry(shapes, {
      depth: 30 * S,
      bevelEnabled: true,
      bevelThickness: 4 * S,
      bevelSize: 3 * S,
      bevelSegments: 4,
      curveSegments: 12,
    });
    geo.computeBoundingBox();
    var bb = geo.boundingBox;
    var textHeight = bb.max.y - bb.min.y;
    geo.translate(
      -0.5 * (bb.max.x + bb.min.x),
      -bb.min.y,
      -0.5 * (bb.max.z + bb.min.z)
    );

    var mat = new THREE.MeshStandardMaterial({
      color: 0xd300ff,
      metalness: 0.9,
      roughness: 0.1,
      emissive: 0x330044,
    });
    var textMesh = new THREE.Mesh(geo, mat);
    textMesh.position.set(0, 0, BG_Z);
    scene.add(textMesh);
    objects.push({ mesh: textMesh, geo: geo, mat: mat });

    // ── Neon edge lines (additive blend for glow look) ──
    var edges = new THREE.EdgesGeometry(geo, 15);
    var neonMat = new THREE.ShaderMaterial({
      uniforms: {
        color: { value: new THREE.Color(0xff88ff) },
        pulse: { value: 1.0 },
      },
      vertexShader: [
        'void main() {',
        '  gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);',
        '}',
      ].join('\n'),
      fragmentShader: [
        'uniform vec3 color;',
        'uniform float pulse;',
        'void main() {',
        '  gl_FragColor = vec4(color * pulse, 1.0);',
        '}',
      ].join('\n'),
      transparent: true,
      blending: THREE.AdditiveBlending,
      depthWrite: false,
    });
    var edgeLines = new THREE.LineSegments(edges, neonMat);
    edgeLines.position.copy(textMesh.position);
    scene.add(edgeLines);
    objects.push({ mesh: edgeLines, geo: edges, mat: neonMat });

    // ── 2 point lights (front + back) ──
    var frontLight = new THREE.PointLight(0xff44ff, 4, 500 * S, 2);
    frontLight.position.set(0, bb.min.y - 5 * S, 20 * S + BG_Z);
    scene.add(frontLight);
    objects.push({ mesh: frontLight });

    var backLight = new THREE.PointLight(0xff44ff, 8, 800 * S, 2);
    backLight.position.set(0, 30 * S, 25 * S + BG_Z);
    scene.add(backLight);
    objects.push({ mesh: backLight });

    // ── Grid lines under text (XZ plane) — extends well beyond view for sense of scale ──
    var gridSize = 120000 * S;
    var gridDiv = 200;
    var gridStep = gridSize / gridDiv;
    var half = gridSize / 2;
    var gridMat = new THREE.LineBasicMaterial({
      color: 0xd300ff, transparent: true, opacity: 0.24,
      blending: THREE.AdditiveBlending, depthWrite: false,
    });
    var gridLines = [];
    for (var gi = 0; gi <= gridDiv; gi++) {
      var pos = -half + gi * gridStep;
      // X-parallel line
      var gx = new THREE.BufferGeometry();
      gx.setAttribute('position', new THREE.BufferAttribute(new Float32Array([
        -half, -textHeight * 0.5, pos + BG_Z, half, -textHeight * 0.5, pos + BG_Z
      ]), 3));
      var lx = new THREE.LineSegments(gx, gridMat);
      scene.add(lx);
      gridLines.push(lx);
      objects.push({ mesh: lx, geo: gx });
      // Z-parallel line
      var gz = new THREE.BufferGeometry();
      gz.setAttribute('position', new THREE.BufferAttribute(new Float32Array([
        pos, -textHeight * 0.5, -half + BG_Z, pos, -textHeight * 0.5, half + BG_Z
      ]), 3));
      var lz = new THREE.LineSegments(gz, gridMat);
      scene.add(lz);
      gridLines.push(lz);
      objects.push({ mesh: lz, geo: gz });
    }
    objects.push({ mat: gridMat });

    // ── Pulse animation (5s cycle) ──
    var PULSE_CYCLE = 5.0;

    this.onFrame = function(dt, t) {
      var fw = (Math.sin(t * Math.PI * 2 / PULSE_CYCLE) + 1) / 2;
      var flashW = 0.3 + 0.7 * fw;
      neonMat.uniforms.pulse.value = flashW * 2.0;
      mat.emissive.setHex(0x8800aa).multiplyScalar(flashW);
      frontLight.intensity = 4.0 * flashW;
      backLight.intensity = 8.0 * flashW;
      gridMat.opacity = 0.15 + 0.25 * fw;
    };
  }.bind(this));

return function() {
  for (var i = 0; i < objects.length; i++) {
    scene.remove(objects[i].mesh);
    if (objects[i].geo) objects[i].geo.dispose();
    if (objects[i].mat) objects[i].mat.dispose();
  }
};
