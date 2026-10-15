#!/bin/sh
# The python kernel host. This script IS the release recipe, run locally: it
# ships SOURCE, and installing it is `uv sync`.
# There is no compile step, so `--profile` changes nothing here — it is accepted
# so every runtime's script takes the same options.
. "$(dirname "$0")/common.sh"
parse_args "$@"
default_out python

say "packaging src/lib/python (source tarball)"
tar --exclude='src/lib/python/.venv' --exclude='*__pycache__*' \
    --exclude='*.pyc' --exclude='*.pytest_cache*' --exclude='*.ruff_cache*' \
    -czf "$OUT" -C "$REPO" src/lib/python
say "wrote $OUT"
echo "$OUT"
