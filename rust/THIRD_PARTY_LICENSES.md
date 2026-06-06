# Third-Party Licenses

The Rust workspace's own code is licensed under AGPL-3.0-or-later (see
`LICENSE` at the repo root). This file lists third-party assets vendored into
the Rust binaries and the terms under which they are redistributed.

All vendored assets are version-pinned. To update one, replace the file
on disk and bump the version line in this document in the same commit.

---

## Three.js

- **Version**: 0.160.0
- **License**: MIT
- **Upstream**: https://github.com/mrdoob/three.js
- **CDN source used for vendoring**: https://cdn.jsdelivr.net/npm/three@0.160.0/build/three.module.min.js
- **Vendored location**: `rust/crates/bundles/fantastic-web/src/assets/three.module.js`
- **Served at**: `/_assets/three.module.js` by `fantastic-web`
- **Consumed by**: `fantastic-canvas-webapp` (canvas surface)

### License text

```
The MIT License

Copyright © 2010-2024 three.js authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

---

## xterm.js

- **Version**: 6.0.0
- **License**: MIT
- **Upstream**: https://github.com/xtermjs/xterm.js
- **CDN sources used for vendoring**:
  - https://cdn.jsdelivr.net/npm/@xterm/xterm@6.0.0/lib/xterm.min.js
  - https://cdn.jsdelivr.net/npm/@xterm/xterm@6.0.0/css/xterm.min.css
- **Vendored locations**:
  - `rust/crates/bundles/fantastic-web/src/assets/xterm.min.js`
  - `rust/crates/bundles/fantastic-web/src/assets/xterm.min.css`
- **Served at**: `/_assets/xterm.min.js`, `/_assets/xterm.min.css` by `fantastic-web`
- **Consumed by**: `fantastic-terminal-webapp` (terminal surface)

### License text

```
Copyright (c) 2017-2024, The xterm.js authors (https://github.com/xtermjs/xterm.js)
Copyright (c) 2014-2016, SourceLair Private Company (https://www.sourcelair.com)
Copyright (c) 2012-2013, Christopher Jeffrey (https://github.com/chjj/)

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

---

## xterm.js fit addon (@xterm/addon-fit)

- **Version**: 0.11.0
- **License**: MIT
- **Upstream**: https://github.com/xtermjs/xterm.js (addons/addon-fit)
- **CDN source used for vendoring**: https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.11.0/lib/addon-fit.min.js
- **Vendored location**: `rust/crates/bundles/fantastic-web/src/assets/xterm-addon-fit.min.js`
- **Served at**: `/_assets/xterm-addon-fit.min.js` by `fantastic-web`
- **Consumed by**: `fantastic-terminal-webapp`

License: same MIT terms as xterm.js above (the addon ships as part of
the xterm.js repository and project).

---

## favicon.png

The bundled favicon at `rust/crates/bundles/fantastic-web/src/favicon.png`
is copied verbatim from the Python web bundle in this same repository
(`python/bundled_agents/web/host/src/web/favicon.png`). Same project,
same AGPL-3.0-or-later license; no third-party attribution needed.
