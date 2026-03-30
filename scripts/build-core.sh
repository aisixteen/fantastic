#!/bin/bash
set -e
cd "$(dirname "$0")/.."

# Build frontend — install deps if needed
if [ -d bundled_agents/canvas/web ]; then
    cd bundled_agents/canvas/web
    [ -d node_modules ] || npm install
    echo "==> Building frontend..."
    npm run build
    cd ../../..
fi

echo "==> Bundling assets into core/_bundled/..."
rm -rf core/_bundled
mkdir -p core/_bundled

[ -d bundled_agents/canvas/web/dist ] && cp -r bundled_agents/canvas/web/dist core/_bundled/web_dist
[ -d skills ] && cp -r skills core/_bundled/skills
[ -d bundled_agents ] && cp -r bundled_agents core/_bundled/agents

# Build terminal bundle assets (source files → dist/)
mkdir -p bundled_agents/terminal/dist
cp bundled_agents/terminal/index.html bundled_agents/terminal/dist/
cp bundled_agents/terminal/bridge.js bundled_agents/terminal/dist/
mkdir -p core/_bundled/agents/terminal/dist
cp bundled_agents/terminal/index.html core/_bundled/agents/terminal/dist/
cp bundled_agents/terminal/bridge.js core/_bundled/agents/terminal/dist/
[ -f CLAUDE.md ] && cp CLAUDE.md core/_bundled/
[ -f fantastic.md ] && cp fantastic.md core/_bundled/

echo "==> Building Python package..."
cd core && uv build

echo "==> Done! Package is in core/dist/"
ls -la dist/*.whl dist/*.tar.gz 2>/dev/null || true

# --install-local: install the wheel into current environment
if [[ "$1" == "--install-local" ]]; then
    echo ""
    echo "==> Installing locally..."
    uv pip install dist/fantastic-*.whl --force-reinstall --no-deps
    echo "==> Installed!"
fi
