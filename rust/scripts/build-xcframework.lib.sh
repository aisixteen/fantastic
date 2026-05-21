#!/usr/bin/env bash
# build-xcframework.lib.sh — shared functions for the XCFramework builders.
#
# Sourced by the two variant scripts:
#   • build-xcframework-embedded.sh  (FantasticLite — sandboxed; no PTY/subprocess)
#   • build-xcframework-full.sh      (FantasticPro  — unsandboxed; terminal + python)
#
# This file is NOT executable on its own; it only defines functions. Each
# variant script:
#   1. Defines its configuration vars (TARGETS, FEATURES, PKG_DIR, XCF_NAME).
#   2. Sources this file.
#   3. Calls `build_xcframework_variant` to run the pipeline.
#
# Conventions:
#   - TARGETS_DEVICE     : Apple-device Rust targets (empty list = skip iOS)
#   - TARGETS_SIM        : Apple-Silicon-simulator Rust targets (skip if not iOS)
#   - TARGETS_MAC        : Mac Rust targets (lipo'd into a fat slice)
#   - FEATURES           : --no-default-features --features <FEATURES> for cargo
#   - PKG_DIR            : packaging/<Name>/ — destination Swift Package
#   - XCF_NAME           : Fantastic-<Variant>.xcframework
#   - SWIFT_MODULE_NAME  : the `import X` name consumers use (e.g. FantasticKernelEmbedded)

set -euo pipefail

# Resolve workspace root from caller's $SCRIPT_DIR (variant scripts set it).
xcf_lib_init() {
    if [[ -z "${SCRIPT_DIR:-}" ]]; then
        echo "build-xcframework.lib: caller must set SCRIPT_DIR before sourcing" >&2
        exit 2
    fi
    RUST_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
    export RUST_ROOT
}

# Sanity-check required CLI tools. Errors with a clear hint.
xcf_check_prereqs() {
    command -v rustup >/dev/null || { echo "build-xcframework: rustup missing"; exit 2; }
    command -v xcodebuild >/dev/null || { echo "build-xcframework: xcodebuild missing (need macOS host)"; exit 2; }
    command -v uniffi-bindgen >/dev/null || {
        echo "build-xcframework: uniffi-bindgen not installed. Run:"
        echo "    cargo install uniffi-bindgen-cli"
        exit 2
    }
}

# Ensure every Rust target is installed via rustup. Fast no-op if already there.
xcf_ensure_targets() {
    local t
    for t in "${TARGETS_DEVICE[@]+"${TARGETS_DEVICE[@]}"}" "${TARGETS_SIM[@]+"${TARGETS_SIM[@]}"}" "${TARGETS_MAC[@]+"${TARGETS_MAC[@]}"}"; do
        rustup target list --installed | grep -qx "$t" || {
            echo "[xcf] installing rustup target: $t"
            rustup target add "$t"
        }
    done
}

# Build the fantastic-uniffi crate for every slice with the configured FEATURES.
xcf_build_slices() {
    local t
    echo "[xcf] building static libs ($FEATURES tier)..."
    cd "$RUST_ROOT"
    for t in "${TARGETS_DEVICE[@]+"${TARGETS_DEVICE[@]}"}" "${TARGETS_SIM[@]+"${TARGETS_SIM[@]}"}" "${TARGETS_MAC[@]+"${TARGETS_MAC[@]}"}"; do
        echo "[xcf]   $t"
        cargo build --release --target "$t" -p fantastic-uniffi \
            --no-default-features --features "$FEATURES"
    done
}

# Run uniffi-bindgen in LIBRARY mode against the Mac slice. Library mode is
# required: #[uniffi::export] proc-macro methods on Kernel live in the .a's
# metadata, NOT the UDL. UDL-mode bindgen produces an empty Kernel class.
#
# Emits Swift bindings + header + modulemap into $BINDINGS_DIR.
xcf_generate_bindings() {
    BINDINGS_DIR="$(mktemp -d)"
    HEADERS_DIR="$BINDINGS_DIR/Headers"
    mkdir -p "$HEADERS_DIR"
    # Library used as the metadata source. Mac-arm64 always built; safe.
    local source_lib="$RUST_ROOT/target/aarch64-apple-darwin/release/libfantastic_uniffi.a"
    echo "[xcf] generating Swift bindings (library mode, from $source_lib)..."
    uniffi-bindgen generate \
        --library "$source_lib" \
        --language swift \
        --out-dir "$BINDINGS_DIR"
    mv "$BINDINGS_DIR/fantasticFFI.h" "$HEADERS_DIR/" 2>/dev/null || true
    cat >"$HEADERS_DIR/module.modulemap" <<'MMAP'
module fantasticFFI {
    header "fantasticFFI.h"
    export *
}
MMAP
}

# Lipo the Mac slices (arm64 + x86_64) into a single fat .a.
# Skipped if TARGETS_MAC has fewer than 2 entries.
xcf_lipo_mac() {
    MAC_FAT="$BINDINGS_DIR/libfantastic_uniffi-macos.a"
    if (( ${#TARGETS_MAC[@]} == 2 )); then
        echo "[xcf] lipo Mac slices..."
        lipo -create \
            "$RUST_ROOT/target/${TARGETS_MAC[0]}/release/libfantastic_uniffi.a" \
            "$RUST_ROOT/target/${TARGETS_MAC[1]}/release/libfantastic_uniffi.a" \
            -output "$MAC_FAT"
    elif (( ${#TARGETS_MAC[@]} == 1 )); then
        cp "$RUST_ROOT/target/${TARGETS_MAC[0]}/release/libfantastic_uniffi.a" "$MAC_FAT"
    else
        MAC_FAT=""    # no Mac slice for this variant
    fi
}

# Stage the iOS simulator slice. Same .a as `aarch64-apple-ios-sim` but
# renamed so xcodebuild can distinguish it from the device slice in
# AvailableLibraries. Skipped if no sim targets.
xcf_stage_sim() {
    SIM_LIB=""
    if (( ${#TARGETS_SIM[@]} >= 1 )); then
        echo "[xcf] iOS sim slice (${TARGETS_SIM[0]})..."
        SIM_LIB="$BINDINGS_DIR/libfantastic_uniffi-iossim.a"
        cp "$RUST_ROOT/target/${TARGETS_SIM[0]}/release/libfantastic_uniffi.a" "$SIM_LIB"
    fi
}

# Assemble the XCFramework from whatever slices got built.
xcf_bundle() {
    local xcf_out="$PKG_DIR/$XCF_NAME"
    echo "[xcf] xcodebuild -create-xcframework → $XCF_NAME ..."
    rm -rf "$xcf_out"
    mkdir -p "$PKG_DIR"

    local args=()
    # Device iOS slice(s).
    if (( ${#TARGETS_DEVICE[@]} >= 1 )); then
        args+=( -library "$RUST_ROOT/target/${TARGETS_DEVICE[0]}/release/libfantastic_uniffi.a"
                -headers "$HEADERS_DIR" )
    fi
    # Sim slice (if staged).
    if [[ -n "$SIM_LIB" ]]; then
        args+=( -library "$SIM_LIB"
                -headers "$HEADERS_DIR" )
    fi
    # Mac (lipo'd or single).
    if [[ -n "$MAC_FAT" ]]; then
        args+=( -library "$MAC_FAT"
                -headers "$HEADERS_DIR" )
    fi

    xcodebuild -create-xcframework "${args[@]}" -output "$xcf_out"
    XCF_OUT="$xcf_out"
}

# Drop the auto-generated Swift bindings into the package's Sources/<Module>/
# directory so consumers see them via `import <SWIFT_MODULE_NAME>`.
xcf_install_bindings() {
    local sources_dir="$PKG_DIR/Sources/$SWIFT_MODULE_NAME"
    mkdir -p "$sources_dir"
    cp "$BINDINGS_DIR/fantastic.swift" "$sources_dir/"
    echo "[xcf] ✓ Swift bindings → $sources_dir/fantastic.swift"
}

# End-to-end orchestration. Variant scripts call this after setting their
# configuration vars and running xcf_lib_init.
build_xcframework_variant() {
    xcf_check_prereqs
    xcf_ensure_targets
    xcf_build_slices
    BINDINGS_DIR=""
    trap '[[ -n "${BINDINGS_DIR:-}" ]] && rm -rf "$BINDINGS_DIR"' EXIT
    xcf_generate_bindings
    xcf_lipo_mac
    xcf_stage_sim
    xcf_bundle
    xcf_install_bindings
    echo
    echo "[xcf] ✓ built $XCF_OUT"
}
