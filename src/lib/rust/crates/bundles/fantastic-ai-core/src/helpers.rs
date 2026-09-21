//! Shared low-level helpers: meta access, client sanitisation, time + id
//! minting.
//!
//! The tool-call id minter that used to live here (`mint_tool_call_id`, no
//! callers) moved to [`crate::tool_parse::mint_call_id`]: the parser is the only
//! thing that mints one, and python keeps `_mint_id` inside `tool_parse.py` for
//! the same reason.
//!
//! The blob-channel file I/O that used to live here (`chat_path` / `file_read` /
//! `file_write` / `file_delete`, all four riding the LOADER's `persist_blob` /
//! `load_blob`) is GONE. Chat was its only caller, and chat now lives in an
//! append-only JSONL sidecar (`agents/<self>/chat_<client>.jsonl`) written
//! through the AI's OWN `file_bridge_id` (see [`crate::history`]) —
//! python-canonical, and independent of whether the loader discovered a
//! persistence provider.

use fantastic_bundle::{is_alnum, py_strip};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::Value;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Headless / REPL caller default. Matches Python's `DEFAULT_CLIENT_ID`.
pub const DEFAULT_CLIENT_ID: &str = "cli";

/// Read a string meta field off the agent record.
pub fn meta_string(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Read a string meta field, falling back to `default`.
pub fn meta_string_or(agent_id: &AgentId, kernel: &Kernel, key: &str, default: &str) -> String {
    meta_string(agent_id, kernel, key).unwrap_or_else(|| default.to_string())
}

/// The bound file agent id, if any.
pub fn file_bridge_id(self_id: &AgentId, kernel: &Kernel) -> Option<String> {
    meta_string(self_id, kernel, "file_bridge_id")
}

/// A snapshot of the agent record's meta map (the per-agent config). Empty
/// if the agent is gone. Used by the context-budget functions, mirroring
/// the Python reference's `rec` dict.
pub fn agent_meta(agent_id: &AgentId, kernel: &Kernel) -> serde_json::Map<String, Value> {
    match kernel.agents.get(agent_id) {
        Some(e) => e.meta.read().expect("meta poisoned").clone(),
        None => serde_json::Map::new(),
    }
}

/// Trim + sanitise a client id so it's safe as a filename suffix — python
/// `_safe_client`, per CODE POINT.
///
/// UNICODE ALPHANUMERICS SURVIVE (`str.isalnum` is Unicode-aware), and there is
/// NO LENGTH CAP. Rust used to keep only ASCII alphanumerics and truncate at 64,
/// so `éx` became `_x` and a long browser client id was silently a DIFFERENT id
/// from python's — two runtimes writing two different chat sidecars for one
/// caller. The strip is python's whitespace set (`\x1c`–`\x1f` included), which
/// `str::trim` does not cover.
pub fn safe_client(client_id: &str) -> String {
    let stripped = py_strip(client_id);
    let raw = if stripped.is_empty() {
        DEFAULT_CLIENT_ID
    } else {
        stripped
    };
    raw.chars()
        .map(|c| {
            if is_alnum(c) || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Current unix time in fractional seconds.
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `n` bytes from the OS CSPRNG — the source class python's `secrets` module
/// is (`os.urandom`). The clock/stack/counter mixer this replaced was not
/// random at all: the capture read `AAAAAVnE3yQ`, a big-endian counter, where
/// python had `cijoBiyMnnw`. A failure is FATAL — there is no second source.
pub(crate) fn os_random(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    // SAFETY: `buf` is a valid writable buffer of exactly `n` bytes.
    let rc = unsafe { libc::getentropy(buf.as_mut_ptr().cast(), n) };
    assert!(rc == 0, "getentropy failed: no randomness source");
    buf
}

/// The base64url alphabet, python's `secrets.token_urlsafe` spelling.
const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Opaque id for a single user submission — python `secrets.token_urlsafe(8)`:
/// 8 bytes as UNPADDED base64url, so 11 characters and NO prefix. Rust minted
/// `snd_<8 hex>`, a shape no python-facing consumer (or capture diff) expects.
pub fn mint_send_id() -> String {
    let bytes = os_random(8);
    let mut out = String::with_capacity(11);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        // 3 bytes → 4 characters, 2 → 3, 1 → 2 (the padding is stripped).
        for i in 0..chunk.len() + 1 {
            let six = (n >> (18 - 6 * i)) & 63;
            out.push(B64URL[six as usize] as char);
        }
    }
    out
}

/// A FRESH 4-hex token for ONE delivered turn's batch — python's
/// `secrets.token_hex(2)`.
///
/// Per TURN and not derived from the batch's content: two turns that made the
/// same calls used to collide (rust/swift hashed `(dest, count, body lengths)`),
/// which is the exact confusion the token exists to prevent — a model that reads
/// the same file twice in a row saw `1 of 2 in batch 7f3a` both times and could
/// not tell the batches apart (measured 2026-08-24, the re-read loop).
pub fn mint_batch_token() -> String {
    os_random(2).iter().map(|b| format!("{b:02x}")).collect()
}
