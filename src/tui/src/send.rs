//! `fantastic send '<item json>'` — ONE call into the kernel running in this
//! directory, from a shell (owner, 2026-10-01: credentials decision 2).
//!
//! - ATTACH-ONLY: it speaks MCP once over the running kernel's MCP channel
//!   (`<state>/cli/mcp.yaml`: a loopback port and a token), as the `mcp` leg; no
//!   live kernel → it fails loud and starts nothing. `--statedir DIR` names the
//!   kernel's state folder, else it is `<current dir>/.fantastic`.
//! - `$env:NAME`: a string value that is exactly `$env:NAME`, anywhere in the
//!   payload, is replaced by THIS shell's `NAME` — the only place one is ever
//!   resolved (the kernel never reads its own env for it). A missing variable
//!   fails before anything is sent.
//! - EVERYTHING ASYNC (#795, owner Q4): it prints the call's ACK —
//!   `{pid, status: accepted}`, or the refusal — and the exit code is 0 when the call
//!   was accepted, 1 otherwise. The result is a row: `ledger {id: <pid>}` reads it,
//!   and the session's settle notification carries it (`fantastic import` reads it
//!   there, [`call_settled`]).

use std::io::Write as _;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use std::path::{Path, PathBuf};

use crate::local::MCP;

const PREFIX: &str = "$env:";

/// Entry point, called from `main` before any runtime exists.
pub fn main(argv: &[String]) -> ! {
    let (statedir, raw) = match argv {
        [raw] => (None, raw),
        [flag, dir, raw] if flag == "--statedir" && !dir.is_empty() => (Some(PathBuf::from(dir)), raw),
        _ => {
            eprintln!("fantastic send: `[--statedir DIR] <item>` — an item: '{{\"target\":…,\"payload\":{{\"type\":…}}}}' (got {} arguments)", argv.len());
            std::process::exit(2);
        }
    };
    let item = match prepare(raw, |n| std::env::var(n).ok()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("fantastic send: {e}");
            std::process::exit(2);
        }
    };
    let state = match fantastic_kernel::bootstrap::state_dir_for(statedir.as_deref(), None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fantastic send: no working directory: {e}");
            std::process::exit(2);
        }
    };
    match fantastic_kernel::lock::held(&state) {
        Ok(Some(pid)) if pid != std::process::id() => {}
        Ok(_) => {
            eprintln!("fantastic send: no kernel runs on this state folder — start `fantastic` or `fantastic mcp` for it first");
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
    let code = match rt.block_on(call(item, &state.join("cli"))) {
        Ok(ack) => {
            let _ = writeln!(std::io::stdout(), "{}", serde_json::to_string_pretty(&ack).unwrap_or_else(|_| ack.to_string()));
            i32::from(ack.get("status").and_then(Value::as_str) != Some("accepted"))
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

/// One MCP session over the kernel's channel in `cli`: initialize, then the `send`
/// tool with the one item. Its ACK back (`{pid, status: accepted}`); the session
/// closes.
pub(crate) async fn call(item: Value, cli: &Path) -> Result<Value, String> {
    let mut s = Session::open(cli).await?;
    let ack = s.send(item).await?;
    s.close().await;
    Ok(ack)
}

/// [`call`], then the call's SETTLE as the session's notification carries it: the
/// item's row `{pid, status, result}`. The shell reads it as a client of the
/// kernel's MCP channel; nothing in the kernel waits for it.
pub(crate) async fn call_settled(item: Value, cli: &Path) -> Result<Value, String> {
    let mut s = Session::open(cli).await?;
    let ack = s.send(item).await?;
    let re = ack.get("pid").cloned().ok_or_else(|| format!("the call was not accepted: {ack}"))?;
    let settle = s.settle(&re).await;
    s.close().await;
    let data = settle?;
    match data.pointer("/outcomes/0") {
        Some(row) => Ok(row.clone()),
        None => Err(format!("the settle carries no row: {data}")),
    }
}

/// One MCP client session on the kernel's channel.
struct Session {
    lines: tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    w: tokio::net::tcp::OwnedWriteHalf,
    /// Settle notifications read before they were asked for: a call that settles at
    /// once may have its settle written before its ack.
    early: Vec<Value>,
}

impl Session {
    async fn open(cli: &Path) -> Result<Self, String> {
        let sock = crate::local::connect(cli, MCP)
            .await
            .map_err(|e| format!("the kernel's MCP channel `{}` does not answer: {e}", cli.join(MCP).display()))?;
        let (r, w) = sock.into_split();
        let mut s = Self { lines: BufReader::new(r).lines(), w, early: Vec::new() };
        s.write(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                          "clientInfo": {"name": "fantastic send", "version": env!("CARGO_PKG_VERSION")}}}))
            .await?;
        s.write(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await?;
        Ok(s)
    }

    async fn write(&mut self, m: &Value) -> Result<(), String> {
        let mut line = m.to_string();
        line.push('\n');
        self.w.write_all(line.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
        self.w.flush().await.map_err(|e| format!("write: {e}"))
    }

    /// The `send` tool with one item: its ack.
    async fn send(&mut self, item: Value) -> Result<Value, String> {
        self.write(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "send", "arguments": {"items": [item]}}}))
            .await?;
        while let Some(line) = self.lines.next_line().await.map_err(|e| format!("read: {e}"))? {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg.get("method").and_then(Value::as_str) == Some(fantastic_web_mcp::SETTLE_METHOD) {
                self.early.push(msg);
                continue;
            }
            if msg.get("id") != Some(&json!(2)) {
                continue;
            }
            if let Some(e) = msg.get("error") {
                return Err(format!("the kernel refused the call: {e}"));
            }
            let text = msg.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or("");
            return match serde_json::from_str::<Value>(text) {
                Ok(ack @ Value::Object(_)) => Ok(ack),
                // a shape error on the item, or the leg's refusal: the tool's message, said as it is
                _ => Err(text.to_string()),
            };
        }
        Err("the kernel closed the channel without an answer".into())
    }

    /// The settle notification naming `re`: its `data`.
    async fn settle(&mut self, re: &Value) -> Result<Value, String> {
        if let Some(at) = self.early.iter().position(|m| m.pointer("/params/re") == Some(re)) {
            let msg = self.early.remove(at);
            return Ok(msg.pointer("/params/data").cloned().unwrap_or(Value::Null));
        }
        while let Some(line) = self.lines.next_line().await.map_err(|e| format!("read: {e}"))? {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg.get("method").and_then(Value::as_str) != Some(fantastic_web_mcp::SETTLE_METHOD) {
                continue;
            }
            if msg.pointer("/params/re") == Some(re) {
                return Ok(msg.pointer("/params/data").cloned().unwrap_or(Value::Null));
            }
        }
        Err("the kernel closed the channel before the call settled".into())
    }

    async fn close(mut self) {
        let _ = self.w.shutdown().await;
    }
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

    /// A fake kernel channel in `cli`: it takes the token, answers the `send` tool with
    /// its ack and pushes the settle — BEFORE the ack when `early` (a call that settles
    /// at once may be written in that order).
    async fn fake_channel(cli: &Path, early: bool) {
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        std::fs::create_dir_all(cli).unwrap();
        std::fs::write(
            cli.join(MCP),
            format!("pid: {}\nport: {port}\ntoken: tok\n", std::process::id()),
        )
        .unwrap();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut r = BufReader::new(r);
            let mut tok = [0u8; 3];
            tokio::io::AsyncReadExt::read_exact(&mut r, &mut tok).await.unwrap();
            let mut lines = r.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let m: Value = serde_json::from_str(&line).unwrap();
                if m["method"] != "tools/call" {
                    continue;
                }
                let ack = json!({"jsonrpc": "2.0", "id": m["id"], "result": {"content": [{"type": "text",
                    "text": json!({"pid": 7, "status": "accepted"}).to_string()}], "isError": false}});
                let note = json!({"jsonrpc": "2.0", "method": fantastic_web_mcp::SETTLE_METHOD,
                    "params": {"type": "settle", "re": 7, "status": "completed",
                               "data": {"outcomes": [{"pid": 8, "status": "completed", "result": {"ok": true}}]}}});
                let order = if early { [note, ack] } else { [ack, note] };
                for v in order {
                    w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
                }
            }
        });
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fc-send-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("cli")
    }

    /// `fantastic send` prints the ACK alone (#795, Q4).
    #[tokio::test]
    async fn send_answers_only_the_ack() {
        let cli = scratch("ack");
        fake_channel(&cli, false).await;
        let ack = call(json!({"target": "t", "payload": {"type": "v"}}), &cli).await.unwrap();
        assert_eq!(ack, json!({"pid": 7, "status": "accepted"}));
    }

    /// The settle is read from the session's notification — also when it was written
    /// before the ack.
    #[tokio::test]
    async fn the_settle_is_read_from_the_notification_in_either_order() {
        for early in [false, true] {
            let cli = scratch(if early { "early" } else { "late" });
            fake_channel(&cli, early).await;
            let row = call_settled(json!({"target": "t", "payload": {"type": "v"}}), &cli).await.unwrap();
            assert_eq!(row, json!({"pid": 8, "status": "completed", "result": {"ok": true}}), "early={early}");
        }
    }

    #[test]
    fn an_item_without_target_or_payload_is_refused() {
        assert!(prepare(r#"{"payload":{"type":"v"}}"#, env).is_err());
        assert!(prepare(r#"{"target":"t"}"#, env).is_err());
        assert!(prepare(r#"[1]"#, env).is_err());
    }
}
