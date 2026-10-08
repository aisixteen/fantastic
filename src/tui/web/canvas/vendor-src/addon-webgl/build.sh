#!/bin/sh
# build.sh — rebuild the vendored, PATCHED @xterm/addon-webgl:
#   src/vendor/addon-webgl.js = @xterm/addon-webgl@0.19.0 (the addon xterm.js
#   tag 6.0.0 ships, byte-for-byte the npm tarball's src/) + fantastic.patch,
#   bundled by the library's vendored esbuild. NO npm, NO node.
#
#   sh build.sh <xterm.js checkout at tag 6.0.0>
#   (git clone --depth 1 --branch 6.0.0 https://github.com/xtermjs/xterm.js.git)
set -eu
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
APP=$(CDPATH= cd -- "$HERE/../.." && pwd)
SRC=${1:?usage: sh build.sh <xterm.js checkout at tag 6.0.0>}
TAG=$(git -C "$SRC" describe --tags --exact-match 2>/dev/null || true)
[ "$TAG" = "6.0.0" ] || { echo "build.sh: $SRC is not xterm.js tag 6.0.0 ($TAG)" >&2; exit 1; }
case "$(uname -sm)" in
  "Darwin arm64") ESB="$APP/../../../lib/ts/tools/esbuild/darwin-arm64/esbuild" ;;
  *) echo "build.sh: no vendored esbuild for $(uname -sm)" >&2; exit 1 ;;
esac
WORK=$(mktemp -d "${TMPDIR:-/tmp}/addon-webgl.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
git -C "$SRC" archive --format=tar HEAD src addons/addon-webgl/src | tar -x -C "$WORK"
( cd "$WORK" && git apply --directory=. "$HERE/fantastic.patch" )
printf '{ "compilerOptions": { "baseUrl": "%s/src", "experimentalDecorators": true, "target": "es2021", "useDefineForClassFields": false } }\n' "$WORK" > "$WORK/tsconfig.json"
"$ESB" "$WORK/addons/addon-webgl/src/WebglAddon.ts" --bundle --format=esm --target=es2021 \
  --tsconfig="$WORK/tsconfig.json" --external:@xterm/xterm --minify --legal-comments=none \
  --outfile="$APP/src/vendor/addon-webgl.js"
shasum -a 256 "$APP/src/vendor/addon-webgl.js"
