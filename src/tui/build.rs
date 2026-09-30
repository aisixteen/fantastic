//! CRITICAL TODO(dev-llm): bakes the dev LLM endpoint into a `--features dev-llm`
//! build, and does nothing at all otherwise.
//!
//! It reads the gitignored `dev.local.yaml`. A missing file or a missing field
//! is a BUILD ERROR naming it: there is no default and no fallback. The whole
//! record reaches the binary as one JSON env var, read by `kern/dev_llm.rs`.
//! The API key is NOT here: it stays in the keychain.

use std::path::Path;

const REQUIRED: [&str; 5] = ["endpoint", "model", "max_tokens", "context_window", "sampling"];

fn main() {
    println!("cargo:rerun-if-changed=dev.local.yaml");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_DEV_LLM").is_none() {
        return;
    }
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let path = Path::new(&dir).join("dev.local.yaml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "dev-llm: cannot read {} ({e}). Copy dev.local.yaml.example to it and fill in the endpoint.",
            path.display()
        )
    });
    let v: serde_json::Value = serde_yaml::from_str(&text)
        .unwrap_or_else(|e| panic!("dev-llm: {} is not valid YAML: {e}", path.display()));
    let obj = v
        .as_object()
        .unwrap_or_else(|| panic!("dev-llm: {} must be a mapping", path.display()));
    for f in REQUIRED {
        match obj.get(f) {
            None | Some(serde_json::Value::Null) => {
                panic!("dev-llm: {} has no `{f}` (required, no default)", path.display())
            }
            Some(serde_json::Value::String(s)) if s.trim().is_empty() || s.starts_with('<') => {
                panic!("dev-llm: `{f}` in {} is still a placeholder", path.display())
            }
            _ => {}
        }
    }
    if obj.contains_key("key") || obj.contains_key("api_key") || obj.contains_key("ANTHROPIC_API_KEY") {
        panic!("dev-llm: {} carries a key — keys live in the keychain, never in a file", path.display());
    }
    println!("cargo:rustc-env=FANTASTIC_DEV_LLM_JSON={}", serde_json::to_string(&v).unwrap());
}
