//! `fantastic send '<item json>'` — ONE call into the kernel running in this
//! directory, from a shell (owner, 2026-10-01: credentials decision 2).
//!
//! - ATTACH-ONLY: it speaks MCP once over the running kernel's socket
//!   (`.fantastic/cli/mcp.sock`), as the `mcp` leg; no live kernel → it fails
//!   loud and starts nothing.
//! - `$env:NAME`: a string value that is exactly `$env:NAME`, anywhere in the
//!   payload, is replaced by THIS shell's `NAME` — the only place one is ever
//!   resolved (the kernel never reads its own env for it). A missing variable
//!   fails before anything is sent.
//! - It prints the item's `{pid, status, result}`; the exit code is 0 when the
//!   item completed, 1 otherwise.

use std::io::Write as _;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::env::Paths;
use crate::mcp::SOCKET;

const PREFIX: &str = "$env:";

/// Entry point, called from `main` before any runtime exists.
pub fn main(argv: &[String]) -> ! {
    let [raw] = argv else {
        eprintln!("fantastic send: one argument — an item: '{{\"target\":…,\"payload\":{{\"type\":…}}}}' (got {})", argv.len());
        std::process::exit(2);
    };
    let item = match prepare(raw, |n| std::env::var(n).ok()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("fantastic send: {e}");
            std::process::exit(2);
        }
    };
    let paths = match Paths::here() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fantastic send: no working directory: {e}");
            std::process::exit(2);
        }
    };
    match fantastic_kernel::lock::held(&paths.cwd) {
        Ok(Some(pid)) if pid != std::process::id() => {}
        Ok(_) => {
            eprintln!("fantastic send: no kernel runs in this directory — start `fantastic` or `fantastic mcp` here first");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("fantastic send: {e}");
            std::process::exit(1);
        }
    }
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fantastic send: no runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = match rt.block_on(call(item)) {
        Ok(row) => {
            let _ = writeln!(std::io::stdout(), "{}", serde_json::to_string_pretty(&row).unwrap_or_else(|_| row.to_string()));
            i32::from(row.get("status").and_then(Value::as_str) != Some("completed"))
        }
        Err(e) => {
            eprintln!("fantastic send: {e}");
            1
        }
    };
    std::process::exit(code);
}

/// Parse the item and resolve every `$env:NAME` in its payload through `env`.
pub fn prepare(raw: &str, env: impl Fn(&str) -> Option<String>) -> Result<Value, String> {
    let mut item: Value = serde_json::from_str(raw).map_err(|e| format!("the item is not JSON: {e}"))?;
    let obj = item.as_object_mut().ok_or("the item must be one object {target, payload}")?;
    if obj.get("target").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err("the item has no `target`".into());
    }
    let payload = obj.get_mut("payload").filter(|p| p.is_object()).ok_or("the item has no `payload` object")?;
    resolve(payload, &env)?;
    Ok(item)
}

fn resolve(v: &mut Value, env: &impl Fn(&str) -> Option<String>) -> Result<(), String> {
    match v {
        Value::String(s) => {
            if let Some(name) = s.strip_prefix(PREFIX) {
                if name.is_empty() {
                    return Err(format!("`{PREFIX}` names no variable"));
                }
                let val = env(name).ok_or_else(|| format!("${name} is not set in this shell — nothing was sent"))?;
                *v = Value::String(val);
            }
        }
        Value::Object(m) => {
            for x in m.values_mut() {
                resolve(x, env)?;
            }
        }
        Value::Array(a) => {
            for x in a.iter_mut() {
                resolve(x, env)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// One MCP session over the socket: initialize, the `send` tool with the one
/// item, its row back.
pub(crate) async fn call(item: Value) -> Result<Value, String> {
    let sock = crate::sys::connect_local(SOCKET)
        .await
        .map_err(|e| format!("the kernel's socket `{SOCKET}` does not answer: {e}"))?;
    let (r, mut w) = sock.into_split();
    let mut lines = BufReader::new(r).lines();
    let msgs = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                          "clientInfo": {"name": "fantastic send", "version": env!("CARGO_PKG_VERSION")}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "send", "arguments": {"items": [item]}}}),
    ];
    for m in msgs {
        let mut line = m.to_string();
        line.push('\n');
        w.write_all(line.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
    }
    w.flush().await.map_err(|e| format!("write: {e}"))?;
    while let Some(line) = lines.next_line().await.map_err(|e| format!("read: {e}"))? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if msg.get("id") != Some(&json!(2)) {
            continue;
        }
        let _ = w.shutdown().await;
        if let Some(e) = msg.get("error") {
            return Err(format!("the kernel refused the call: {e}"));
        }
        let text = msg.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or("");
        return match serde_json::from_str::<Value>(text) {
            Ok(Value::Array(mut rows)) if rows.len() == 1 => Ok(rows.remove(0)),
            // a shape error on the item: the tool's message, said as it is
            _ => Err(text.to_string()),
        };
    }
    Err("the kernel closed the socket without an answer".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(n: &str) -> Option<String> {
        (n == "KEY").then(|| "sk-123".to_string())
    }

    #[test]
    fn an_env_value_is_resolved() {
        let item = prepare(
            r#"{"target":"credentials","payload":{"type":"set","ref":"r","fields":{"api_key":"$env:KEY"},"a":["$env:KEY"]}}"#,
            env,
        )
        .unwrap();
        assert_eq!(item["payload"]["fields"]["api_key"], "sk-123");
        assert_eq!(item["payload"]["a"][0], "sk-123");
    }

    #[test]
    fn a_missing_variable_fails_before_sending() {
        let e = prepare(r#"{"target":"t","payload":{"type":"v","k":"$env:NOPE"}}"#, env).unwrap_err();
        assert!(e.contains("$NOPE is not set"), "{e}");
    }

    #[test]
    fn only_a_whole_value_is_resolved() {
        let item = prepare(r#"{"target":"t","payload":{"type":"v","k":"x $env:KEY"}}"#, env).unwrap();
        assert_eq!(item["payload"]["k"], "x $env:KEY");
    }

    #[test]
    fn an_item_without_target_or_payload_is_refused() {
        assert!(prepare(r#"{"payload":{"type":"v"}}"#, env).is_err());
        assert!(prepare(r#"{"target":"t"}"#, env).is_err());
        assert!(prepare(r#"[1]"#, env).is_err());
    }
}
