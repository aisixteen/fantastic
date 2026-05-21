//! Plugin trait re-exported by every fantastic_canvas Rust bundle.
//!
//! Mirrors the Python contract: a bundle answers verbs via a handler,
//! optionally implements `on_delete` / `on_shutdown` lifecycle hooks.
//! The CLI links bundles in at compile time (entry-point discovery
//! doesn't exist in Rust; iOS forbids dynamic loading anyway).
//!
//! Phase 1 stub — the real trait lands alongside the kernel substrate
//! impl (task #228). Keeping the crate present so the workspace
//! compiles end-to-end and downstream bundles can declare a
//! `fantastic-bundle` dep without a missing-crate error.

#![deny(missing_docs)]

/// Marker trait — full async signature lands with the kernel substrate.
pub trait BundlePlaceholder {}
