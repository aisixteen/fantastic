//! Builds and links the swift `FantasticFMBridge` static library — the C ABI over
//! `FantasticFMCore`, THE ONE implementation of the Apple Foundation Models
//! calls. The package is this crate's own `swift/` directory.
//!
//! ON A macOS TARGET ONLY:
//!   1. `swift build -c release --product FantasticFMBridge` over
//!      the crate's `swift/` package, into `$OUT_DIR/swift-build` (`--scratch-path`) — the
//!      swift package's own `.build` is never touched;
//!   2. the archive is linked statically, with the Foundation and
//!      FoundationModels frameworks and the Swift runtime. Paths come from the
//!      toolchain (`xcrun`), never from a fixed location. FoundationModels ends up
//!      WEAK in the final binary (every symbol the bridge takes from it is
//!      `@available(macOS 26)` against a macOS 14 deployment), so a binary still
//!      loads on a macOS without it — and the bridge refuses there, naming it.
//!
//! A failing swift build FAILS this build, loudly, naming what failed. There is
//! no fallback: nothing is skipped, stubbed or routed elsewhere.
//!
//! ON EVERY OTHER TARGET nothing is built or linked; the crate compiles its
//! refusing engine (`platform not macOS 26+ — this is <X>`).

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The macOS major version the swift package targets (`.macOS(.v14)`).
const MACOS_MIN: u32 = 14;

/// The swift sources the bridge is built from, relative to the swift package.
const SWIFT_INPUTS: [&str; 3] = [
    "Sources/FantasticFMBridge",
    "Sources/FantasticFMCore",
    "Package.swift",
];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // the swift package lives in this crate: crates/bundles/<this>/swift
    let swift_pkg = manifest.join("swift");
    for rel in SWIFT_INPUTS {
        println!("cargo:rerun-if-changed={}", swift_pkg.join(rel).display());
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    check_deployment_target();
    let swift_pkg = swift_pkg.canonicalize().unwrap_or_else(|e| {
        fail(&format!(
            "the swift package is missing at {}: {e}",
            swift_pkg.display()
        ))
    });
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let scratch = out_dir.join("swift-build");
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        other => fail(&format!(
            "no swift architecture for the rust target arch {other:?} (arm64 and x86_64 are built)"
        )),
    };

    let swift = xcrun(&["--find", "swift"]);
    let sdk = xcrun(&["--sdk", "macosx", "--show-sdk-path"]);
    let build_args = |extra: &[&str]| -> Vec<String> {
        let mut args: Vec<String> = [
            "build",
            "-c",
            "release",
            "--product",
            "FantasticFMBridge",
            "--arch",
            arch,
            "--package-path",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        args.push(swift_pkg.display().to_string());
        args.push("--scratch-path".to_string());
        args.push(scratch.display().to_string());
        args.extend(extra.iter().map(|s| (*s).to_string()));
        args
    };

    let out = Command::new(&swift)
        .args(build_args(&[]))
        .output()
        .unwrap_or_else(|e| fail(&format!("could not run {swift}: {e}")));
    if !out.status.success() {
        fail(&format!(
            "`{swift} {}` failed ({}):\n{}\n{}",
            build_args(&[]).join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let bin = Command::new(&swift)
        .args(build_args(&["--show-bin-path"]))
        .output()
        .unwrap_or_else(|e| fail(&format!("could not run {swift}: {e}")));
    if !bin.status.success() {
        fail(&format!(
            "`swift build --show-bin-path` failed: {}",
            String::from_utf8_lossy(&bin.stderr)
        ));
    }
    let bin = PathBuf::from(String::from_utf8_lossy(&bin.stdout).trim());
    let archive = bin.join("libFantasticFMBridge.a");
    if !archive.is_file() {
        fail(&format!(
            "the swift build reported success but {} is not there",
            archive.display()
        ));
    }
    let swift_runtime = Path::new(&sdk).join("usr/lib/swift");
    if !swift_runtime.join("libswiftCore.tbd").is_file() {
        fail(&format!(
            "the Swift runtime is not in the macOS SDK at {}",
            swift_runtime.display()
        ));
    }

    println!("cargo:rustc-link-search=native={}", bin.display());
    println!("cargo:rustc-link-lib=static=FantasticFMBridge");
    // The Swift runtime the bridge's objects autolink (`-lswiftCore`,
    // `-lswift_Concurrency`, …): the SDK's stubs, whose install names are the
    // OS's own `/usr/lib/swift`, so no rpath is needed.
    println!("cargo:rustc-link-search=native={}", swift_runtime.display());
    println!("cargo:rustc-link-lib=dylib=swiftCore");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=FoundationModels");
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
}

/// The macOS the final binary is linked for must be the bridge's own (14.0) or
/// later. Below 12.0 the Swift runtime the bridge needs links through `@rpath`
/// and the binary aborts at launch in dyld; below 14.0 the binary claims to run
/// where the swift objects cannot. rustc links for 11.0 unless
/// `MACOSX_DEPLOYMENT_TARGET` says otherwise — `src/lib/rust/.cargo/config.toml`
/// sets it for this workspace; any other workspace linking this crate must too.
fn check_deployment_target() {
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let v = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_default();
    let major = v.split('.').next().and_then(|m| m.parse::<u32>().ok());
    if !major.is_some_and(|m| m >= MACOS_MIN) {
        fail(&format!(
            "MACOSX_DEPLOYMENT_TARGET must be {MACOS_MIN}.0 or later (the swift \
             FantasticFMBridge is built for macOS {MACOS_MIN}); it is {:?}. Set it in the \
             environment or in the workspace's .cargo/config.toml [env] (src/lib/rust's \
             sets 14.0).",
            if v.is_empty() {
                "unset (rustc's 11.0)"
            } else {
                v.as_str()
            }
        ));
    }
}

/// One `xcrun` answer, trimmed — fails the build naming the query.
fn xcrun(args: &[&str]) -> String {
    let out = Command::new("xcrun")
        .args(args)
        .output()
        .unwrap_or_else(|e| fail(&format!("could not run xcrun {}: {e}", args.join(" "))));
    if !out.status.success() {
        fail(&format!(
            "`xcrun {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Fail the cargo build, loudly — no fallback.
fn fail(msg: &str) -> ! {
    panic!("fantastic-foundation-models-backend: {msg}");
}
