#!/bin/sh
# Fail if a lockfile pins a trust store that still carries the compromised
# emSign ECC Root CA - G3 (#280): webpki-roots >= 1.0.10, certifi >= 2026.10.19.
set -eu
cd "$(git rev-parse --show-toplevel)"
status=0

check() {
  file=$1; pkg=$2; min=$3
  for v in $(awk -v p="$pkg" '
    $0 == "name = \"" p "\"" { getline l; sub(/^version = "/, "", l); sub(/"$/, "", l); print l }
  ' "$file"); do
    lo=$(printf '%s\n%s\n' "$min" "$v" | sort -V | head -1)
    if [ "$lo" != "$min" ]; then
      echo "$file: $pkg $v trusts the compromised emSign root (need >= $min)"
      status=1
    fi
  done
}

for f in $(git ls-files '*Cargo.lock'); do
  check "$f" webpki-roots 1.0.10
done
for f in $(git ls-files '*uv.lock'); do
  check "$f" certifi 2026.10.19
done
exit $status
