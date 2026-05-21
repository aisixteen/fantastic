#!/usr/bin/env bash
# Rust code-quality sweep — runs the tooling stack the May-2026 survey
# recommended. Each tool runs in its own section so failures are
# attributed cleanly. Hard failures (clippy warnings, fmt diff, test
# failures, security advisories) exit non-zero; informational findings
# (unused deps, outdated crates, public-API drift) print a warning
# and continue.
#
# Usage:
#   ./scripts/quality.sh                # run with whatever's installed; skip missing
#   ./scripts/quality.sh --install      # cargo-install missing tools first
#   ./scripts/quality.sh --strict       # informational findings also fail
#   ./scripts/quality.sh --section deny # run only that section (debug)
#
# Sections (run in order):
#   compile     cargo check --workspace + embedded feature gate
#   fmt         cargo fmt --all -- --check
#   clippy      cargo clippy --workspace --all-targets -- -D warnings
#   test        cargo test --workspace
#   deny        cargo-deny  (advisories + licenses + duplicate-versions)
#   audit       cargo-audit (RustSec CVE only — redundant with deny but cheap)
#   machete     cargo-machete (unused deps)
#   outdated    cargo-outdated (informational)
#   miri        cargo miri test (only if any crate uses `unsafe`)
#   tree        cargo tree --duplicates (informational)
#
# Skips any tool that isn't installed unless --install was passed.
# Logs land in /tmp/fantastic-quality.<section>.log on failure for
# postmortem.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RUST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$RUST_DIR"

# `cargo install` drops binaries into `~/.cargo/bin/`. Many shells
# (especially non-login zsh on macOS) don't include that on PATH by
# default. Prepend it so freshly-installed tools are visible without
# requiring a `rustup-init` PATH-edit. Idempotent: contains-check
# avoids ballooning PATH on repeated invocations.
if [ -d "$HOME/.cargo/bin" ] && ! echo ":$PATH:" | grep -q ":$HOME/.cargo/bin:"; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

INSTALL=0
STRICT=0
ONLY_SECTION=""
for arg in "$@"; do
    case "$arg" in
        --install) INSTALL=1 ;;
        --strict)  STRICT=1 ;;
        --section) ;;  # consumed by next iter
        --section=*) ONLY_SECTION="${arg#--section=}" ;;
        --help|-h)
            sed -n '2,40p' "$0"
            exit 0
            ;;
        *)
            if [ -n "${PREV_FLAG:-}" ] && [ "$PREV_FLAG" = "--section" ]; then
                ONLY_SECTION="$arg"
                PREV_FLAG=""
            else
                echo "unknown arg: $arg" >&2; exit 2
            fi
            ;;
    esac
    if [ "$arg" = "--section" ]; then PREV_FLAG="--section"; fi
done

# ─── color helpers ───────────────────────────────────────────────
RED='\033[31m'; GREEN='\033[32m'; YELLOW='\033[33m'; CYAN='\033[36m'; DIM='\033[2m'; RESET='\033[0m'
if [ ! -t 1 ]; then RED=''; GREEN=''; YELLOW=''; CYAN=''; DIM=''; RESET=''; fi

# Result tracking.
declare -a PASS FAIL WARN SKIP

run_section() {
    local name="$1"; shift
    [ -n "$ONLY_SECTION" ] && [ "$ONLY_SECTION" != "$name" ] && return 0
    printf "\n${CYAN}── %s ───────────────────────────────────────────────${RESET}\n" "$name"
    local log="/tmp/fantastic-quality.${name}.log"
    "$@" 2>&1 | tee "$log"
    local rc=${PIPESTATUS[0]}
    if [ "$rc" -eq 0 ]; then
        printf "${GREEN}✓ %s PASS${RESET}\n" "$name"
        PASS+=("$name")
        rm -f "$log"
    else
        printf "${RED}✗ %s FAIL (rc=%d, log=%s)${RESET}\n" "$name" "$rc" "$log"
        FAIL+=("$name")
    fi
    return 0
}

run_section_soft() {
    local name="$1"; shift
    [ -n "$ONLY_SECTION" ] && [ "$ONLY_SECTION" != "$name" ] && return 0
    printf "\n${CYAN}── %s (informational) ──────────────────────────────${RESET}\n" "$name"
    local log="/tmp/fantastic-quality.${name}.log"
    "$@" 2>&1 | tee "$log"
    local rc=${PIPESTATUS[0]}
    if [ "$rc" -eq 0 ]; then
        printf "${GREEN}✓ %s clean${RESET}\n" "$name"
        PASS+=("$name")
        rm -f "$log"
    else
        if [ "$STRICT" -eq 1 ]; then
            printf "${RED}✗ %s findings (strict mode treats as fail; log=%s)${RESET}\n" "$name" "$log"
            FAIL+=("$name")
        else
            printf "${YELLOW}⚠ %s findings (informational; log=%s)${RESET}\n" "$name" "$log"
            WARN+=("$name")
        fi
    fi
    return 0
}

# Skip a section + record reason.
skip_section() {
    local name="$1" reason="$2"
    [ -n "$ONLY_SECTION" ] && [ "$ONLY_SECTION" != "$name" ] && return 0
    printf "\n${CYAN}── %s ───────────────────────────────────────────────${RESET}\n" "$name"
    printf "${DIM}skipped: %s${RESET}\n" "$reason"
    SKIP+=("$name ($reason)")
}

# Tool-present check; install on demand if --install was passed.
ensure_tool() {
    local tool="$1" install_cmd="$2"
    if command -v "$tool" >/dev/null 2>&1; then return 0; fi
    if [ "$INSTALL" -eq 1 ]; then
        printf "${YELLOW}  installing %s …${RESET}\n" "$tool"
        eval "$install_cmd" >/dev/null
        command -v "$tool" >/dev/null 2>&1 && return 0
        return 1
    fi
    return 1
}

# ─── sections ───────────────────────────────────────────────────

run_section compile bash -c '
set -e
cargo check --workspace
cargo check -p fantastic-cli    --no-default-features --features embedded
cargo check -p fantastic-uniffi --no-default-features --features embedded
'

run_section fmt cargo fmt --all -- --check

run_section clippy cargo clippy --workspace --all-targets -- -D warnings

run_section test cargo test --workspace

# cargo-deny — security + licenses + dupes
if ensure_tool cargo-deny "cargo install cargo-deny --locked"; then
    if [ ! -f deny.toml ]; then
        printf "${DIM}  no deny.toml; writing a minimal one to /tmp/fantastic-deny.toml${RESET}\n"
        cat > /tmp/fantastic-deny.toml <<'TOML'
# Minimal cargo-deny config — strict on advisories + duplicates,
# permissive on licenses. Tighten before publishing.
[advisories]
db-path = "~/.cargo/advisory-db"
db-urls = ["https://github.com/RustSec/advisory-db"]
yanked = "warn"

[licenses]
# Common OSS licenses; expand as needed.
allow = ["MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Unicode-3.0", "Unicode-DFS-2016", "CC0-1.0", "Zlib", "MPL-2.0", "MIT-0", "0BSD", "OpenSSL"]
confidence-threshold = 0.8

[bans]
multiple-versions = "warn"
wildcards = "deny"

[sources]
unknown-registry = "warn"
unknown-git = "warn"
TOML
        run_section_soft deny cargo deny --config /tmp/fantastic-deny.toml check
    else
        run_section_soft deny cargo deny check
    fi
else
    skip_section deny "cargo-deny not installed (--install to add)"
fi

# cargo-audit — RustSec only; runs even if deny ran (cheap)
if ensure_tool cargo-audit "cargo install cargo-audit --locked"; then
    run_section audit cargo audit
else
    skip_section audit "cargo-audit not installed (--install to add)"
fi

# cargo-machete — unused deps
if ensure_tool cargo-machete "cargo install cargo-machete --locked"; then
    run_section_soft machete cargo machete
else
    skip_section machete "cargo-machete not installed (--install to add)"
fi

# cargo-outdated — informational
if ensure_tool cargo-outdated "cargo install cargo-outdated --locked"; then
    run_section_soft outdated cargo outdated --workspace --depth 1
else
    skip_section outdated "cargo-outdated not installed (--install to add)"
fi

# miri — only if any crate uses `unsafe`. Cheap grep first.
if grep -rE '\bunsafe\b' --include='*.rs' crates/ >/dev/null 2>&1; then
    if rustup +nightly --help >/dev/null 2>&1 && rustup +nightly component list --installed 2>/dev/null | grep -q miri; then
        run_section miri cargo +nightly miri test --workspace
    else
        skip_section miri "needs nightly + miri component: rustup toolchain install nightly && rustup +nightly component add miri"
    fi
else
    skip_section miri "no \`unsafe\` blocks detected — skipped"
fi

# cargo tree --duplicates — informational
run_section_soft tree cargo tree --workspace --duplicates --depth 1

# ─── summary ────────────────────────────────────────────────────
printf "\n${CYAN}════════════════════════════════════════════════════════════${RESET}\n"
printf "${CYAN}Quality sweep summary${RESET}\n"
printf "${CYAN}════════════════════════════════════════════════════════════${RESET}\n"

printf "${GREEN}PASS${RESET}: %d  " "${#PASS[@]}";  [ "${#PASS[@]}" -gt 0 ] && printf "(%s)" "$(IFS=', '; echo "${PASS[*]}")"; printf "\n"
printf "${YELLOW}WARN${RESET}: %d  " "${#WARN[@]}"; [ "${#WARN[@]}" -gt 0 ] && printf "(%s)" "$(IFS=', '; echo "${WARN[*]}")"; printf "\n"
printf "${RED}FAIL${RESET}: %d  " "${#FAIL[@]}";   [ "${#FAIL[@]}" -gt 0 ] && printf "(%s)" "$(IFS=', '; echo "${FAIL[*]}")"; printf "\n"
printf "${DIM}SKIP${RESET}: %d  " "${#SKIP[@]}";   [ "${#SKIP[@]}" -gt 0 ] && printf "(%s)" "$(IFS='; '; echo "${SKIP[*]}")"; printf "\n"

if [ "${#FAIL[@]}" -gt 0 ]; then
    printf "\n${RED}One or more sections failed.${RESET} Logs in /tmp/fantastic-quality.*.log\n"
    exit 1
fi
exit 0
