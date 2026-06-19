#!/usr/bin/env bash
# Local pre-flight for a Rust binary release.
#
# Usage:   ./rust/scripts/release.sh <version>
# Example: ./rust/scripts/release.sh 0.1.0
#
# Does:
#   1. Verify clean working tree + on `main`
#   2. Bump `rust/Cargo.toml [workspace.package].version`
#   3. Run `./rust/scripts/quality.sh` (8/8 PASS required)
#   4. Print the exact `git tag` + `git push` commands to fire CI
#
# Does NOT push anything. The trigger stays manual — operator copies
# the two commands and runs them to fire `.github/workflows/release-rust.yml`.

set -uo pipefail

if [ $# -ne 1 ]; then
    echo "usage: $0 <version>  (e.g. 0.1.0, 0.2.0-rc1)" >&2
    exit 2
fi
VERSION="$1"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RUST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_DIR="$(cd "$RUST_DIR/.." && pwd)"
cd "$REPO_DIR"

# ─── color ───────────────────────────────────────────────────────
RED='\033[31m'; GREEN='\033[32m'; YELLOW='\033[33m'; CYAN='\033[36m'; DIM='\033[2m'; RESET='\033[0m'
if [ ! -t 1 ]; then RED=''; GREEN=''; YELLOW=''; CYAN=''; DIM=''; RESET=''; fi

step() { printf "\n${CYAN}── %s${RESET}\n" "$1"; }
ok()   { printf "${GREEN}✓ %s${RESET}\n" "$1"; }
err()  { printf "${RED}✗ %s${RESET}\n" "$1"; exit 1; }

# ─── 1. clean tree + on main ────────────────────────────────────
step "1. branch + working tree"
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
echo "   branch: $BRANCH"
if [ "$BRANCH" != "main" ]; then
    printf "${YELLOW}⚠ not on main — releases should normally cut from main${RESET}\n"
    printf "${YELLOW}  Continue anyway? [y/N] ${RESET}"
    read -r reply
    case "$reply" in [yY]*) ;; *) err "aborted" ;; esac
fi
if ! git diff --quiet || ! git diff --cached --quiet; then
    err "working tree dirty — commit or stash first"
fi
ok "tree clean, on branch '$BRANCH'"

# ─── 2. bump version ────────────────────────────────────────────
step "2. version bump → $VERSION"
CARGO_TOML="$RUST_DIR/Cargo.toml"
CURRENT="$(grep -E '^version = ' "$CARGO_TOML" | head -1 | sed -E 's/version = "(.*)"/\1/')"
echo "   current: $CURRENT"
echo "   target:  $VERSION"
if [ "$CURRENT" = "$VERSION" ]; then
    printf "${YELLOW}⚠ version already at $VERSION — skipping bump${RESET}\n"
else
    # Bump only the workspace.package.version line. Macos sed quirk: -i ''
    sed -i.bak -E "s/^version = \"$CURRENT\"/version = \"$VERSION\"/" "$CARGO_TOML"
    rm -f "$CARGO_TOML.bak"
    NEW="$(grep -E '^version = ' "$CARGO_TOML" | head -1 | sed -E 's/version = "(.*)"/\1/')"
    [ "$NEW" = "$VERSION" ] || err "version bump failed (still $NEW)"
    ok "version bumped"
    # Rebuild Cargo.lock so the version change is reflected.
    (cd "$RUST_DIR" && cargo check --workspace >/dev/null 2>&1) || err "cargo check failed after bump"
    ok "Cargo.lock refreshed"
fi

# ─── 3. quality sweep ────────────────────────────────────────────
step "3. quality.sh"
if ! "$RUST_DIR/scripts/quality.sh"; then
    err "quality.sh failed — fix before tagging"
fi
ok "quality sweep clean"

# ─── 4. print tag + push commands ───────────────────────────────
step "4. ready"
TAG="rust-v$VERSION"
printf "\n${GREEN}All checks passed.${RESET} To fire the release workflow:\n\n"
if [ "$CURRENT" != "$VERSION" ]; then
    printf "  ${CYAN}# 1. commit the version bump${RESET}\n"
    printf "  git add rust/Cargo.toml rust/Cargo.lock\n"
    printf "  git commit -m \"rust: bump to v$VERSION\"\n\n"
fi
printf "  ${CYAN}# 2. tag + push (this fires .github/workflows/release-rust.yml)${RESET}\n"
printf "  git tag %s\n" "$TAG"
printf "  git push origin %s\n" "$TAG"
printf "  ${DIM}# (also push the commit if you didn't already: git push)${RESET}\n\n"
printf "Artifacts will land at:\n"
printf "  https://github.com/Alexadar/fantastic_canvas/releases/tag/%s\n" "$TAG"
printf "  https://github.com/Alexadar/fantastic_canvas/releases/latest/download/fantastic-rust-<triple>.tar.gz\n"
