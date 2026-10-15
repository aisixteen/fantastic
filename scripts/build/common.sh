# Shared by every `scripts/build/<runtime>.sh`. Sourced, never executed.
#
# ONE PLACE knows how to build each runtime's artifact. The release (built
# locally) and the e2e harness both call these scripts, so "e2e runs what we ship" is true by
# construction rather than by two lists that drift.
#
# Every script takes the same two options and prints the artifact path it wrote:
#
#   --profile debug|release   default: release
#   --out <path>              default: dist/fantastic-<runtime>-<profile>.<ext>
#
# `release` is what the published artifact is built with; `debug` is the SAME
# command, product and packaging with the optimiser off (and, for ts, the
# minifier off). Nothing else differs.

set -eu

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
PROFILE=release
OUT=""

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --profile) PROFILE="${2:?--profile needs a value}"; shift 2 ;;
      --out)     OUT="${2:?--out needs a value}"; shift 2 ;;
      -h|--help) usage; exit 0 ;;
      *) echo "$(basename "$0"): unknown option '$1'" >&2; usage >&2; exit 2 ;;
    esac
  done
  case "$PROFILE" in
    debug|release) ;;
    *) echo "$(basename "$0"): --profile must be debug|release, got '$PROFILE'" >&2; exit 2 ;;
  esac
}

usage() {
  echo "usage: $(basename "$0") [--profile debug|release] [--out <path>]"
}

# Default artifact path for a runtime, if --out was not given.
default_out() {
  [ -n "$OUT" ] || OUT="$REPO/dist/fantastic-$1-$PROFILE.${2:-tar.gz}"
  mkdir -p "$(dirname "$OUT")"
  # ABSOLUTE, ALWAYS. A recipe may `cd` into its product dir to pack (swift has
  # to, so the shell expands `*.bundle` there), and a relative `--out` would then
  # be written somewhere nobody asked for. Resolve it once, here, rather than
  # making every recipe remember.
  OUT="$(cd "$(dirname "$OUT")" && pwd)/$(basename "$OUT")"
}

say() { echo "build/$(basename "$0" .sh): $*"; }
