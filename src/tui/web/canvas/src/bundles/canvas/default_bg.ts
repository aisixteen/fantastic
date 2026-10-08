// THE BUNDLED BACKGROUND (owner 2026-10-08): the GL source `default_bg` restores
// on a view's background. Plain black for now — the theme set replaces it.
export const DEFAULT_BG_SOURCE =
  "const m = new THREE.ShaderMaterial({depthTest: false, depthWrite: false, " +
  "vertexShader: 'void main(){gl_Position=vec4(position.xy,0.,1.);}', " +
  "fragmentShader: 'void main(){gl_FragColor=vec4(0.,0.,0.,1.);}'});\n" +
  "const q = new THREE.Mesh(new THREE.PlaneGeometry(2, 2), m);\n" +
  "q.frustumCulled = false; q.renderOrder = -1;\n" +
  "scene.add(q);\n" +
  "cleanup(() => { q.geometry.dispose(); m.dispose(); });";
