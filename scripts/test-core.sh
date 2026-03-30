#!/bin/bash
set -e
cd "$(dirname "$0")/.."

echo "==> Running backend tests..."
cd core && uv run pytest tests/ -v -x "$@"
cd ..

echo ""
echo "==> Running frontend tests..."
cd bundled_agents/canvas/web && npx vitest run

echo ""
echo "==> All tests passed!"
