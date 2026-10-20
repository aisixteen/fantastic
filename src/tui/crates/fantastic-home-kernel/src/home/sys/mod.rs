//! THE OS LAYER: every OS call of this crate, and nothing else. A function that
//! is the same on every OS family is written once here; the ones that differ are
//! in `unix.rs` and `windows.rs`, which export the same names, re-exported below
//! the way `src/tui/src/sys/mod.rs` does it.
//!
//! `tests/os_boundary.rs` holds the rest of the crate to that: outside this
//! directory and `src/bin/`, no source names the file system, processes, the
//! network, the environment, signals or the wall clock.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fantastic_home::{Canvas, CanvasRun, RUN_FILE};
use fantastic_kernel::lock;
use fantastic_user_home_store::user_store::{fantastics_dir, INFO};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(test)]
mod tests;

/// How long a canvas has to answer its page's GET.
pub const CANVAS_UP_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long a kernel has to answer `reflect` with its guid.
pub const GUID_TIMEOUT: Duration = Duration::from_secs(3);
/// How many lines of a launch log `log_tail` keeps, and how many characters.
const TAIL_LINES: usize = 4;
const TAIL_CHARS: usize = 600;

/// The wall clock, in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// This process's id.
pub fn pid() -> u32 {
    std::process::id()
}

/// Does `path` name a file.
pub fn is_file(path: &Path) -> bool {
    path.is_file()
}

/// Does anything exist at `path`.
pub fn exists(path: &Path) -> bool {
    path.exists()
}

/// Write `bytes` to `path` whole or not at all: the parent dir created, then
/// `<path>.tmp.<pid>` written and renamed over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}", pid()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Remove the file at `path`; a missing file is already removed.
pub fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// A port on 127.0.0.1 that is free now (the OS picks it).
pub fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("no free port to pick: {e}"))?;
    l.local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("no free port to pick: {e}"))
}

/// The registry's `(folder name, fantastic.yaml text)` entries. No registry dir
/// is an empty registry; a folder without a readable UTF-8 file is skipped.
pub fn read_registry(home: &Path) -> Result<Vec<(String, String)>, String> {
    let dir = fantastics_dir(home);
    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path.join(INFO)) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        out.push((name, text));
    }
    Ok(out)
}

/// Does `path/.fantastic/` exist as a directory.
pub fn is_fantastic(path: &str) -> bool {
    Path::new(path).join(".fantastic").is_dir()
}

/// The live pid holding `path`'s lock. A stale file (its pid dead) is not asked
/// for its flock: a probe's shared flock on it would refuse a kernel taking it
/// over at that moment.
pub fn lock_holder(path: &str) -> Option<u32> {
    let p = Path::new(path);
    match lock::current_holder(p) {
        Ok(Some(pid)) if lock::pid_alive(pid) => match lock::held(p) {
            Ok(holder) => holder,
            Err(e) => {
                tracing::warn!("{path}: the lock: {e}");
                None
            }
        },
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("{path}: the lock: {e}");
            None
        }
    }
}

/// `path`'s run file, parsed; unreadable or invalid is `None`.
pub fn run_file(path: &str) -> Option<CanvasRun> {
    let text = std::fs::read_to_string(Path::new(path).join(RUN_FILE)).ok()?;
    CanvasRun::parse(&text)
}

/// The home host's run file `W/home.yaml`, as written while it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRun {
    pub pid: u32,
    pub port: u16,
    pub url: String,
}

/// The home host's run file path: `<home>/app/home.yaml`.
pub fn host_run_file(home: &Path) -> PathBuf {
    home.join("app").join("home.yaml")
}

/// Read `<home>/app/home.yaml`: `pid` a u32, `port` in 1..=65535 and
/// `url` an `http://127.0.0.1:<port>/…` string. Anything else is `None`.
pub fn read_host_run_file(home: &Path) -> Option<HostRun> {
    let text = std::fs::read_to_string(host_run_file(home)).ok()?;
    parse_host_run_file(&text)
}

/// The run file's text, parsed (see [`read_host_run_file`]).
pub fn parse_host_run_file(text: &str) -> Option<HostRun> {
    let doc: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
    let map = doc.as_mapping()?;
    let pid = u32::try_from(map.get("pid")?.as_u64()?).ok()?;
    let port = u16::try_from(map.get("port")?.as_u64()?)
        .ok()
        .filter(|p| *p != 0)?;
    let url = map.get("url")?.as_str()?.to_string();
    if !url.starts_with(&format!("http://127.0.0.1:{port}/")) {
        return None;
    }
    Some(HostRun { pid, port, url })
}

/// Does `canvas` answer a GET of its page with a 2xx or 3xx status line, within
/// 1.5 s. A refused connection, a timeout or a malformed line is `false`.
pub async fn canvas_up(canvas: &Canvas) -> bool {
    tokio::time::timeout(CANVAS_UP_TIMEOUT, get_status(canvas))
        .await
        .unwrap_or(false)
}

async fn get_status(canvas: &Canvas) -> bool {
    let Some(rest) = canvas.url.strip_prefix("http://") else {
        return false;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let mut target = rest[end..].split('#').next().unwrap_or("").to_string();
    if target.is_empty() || target.starts_with('?') {
        target.insert(0, '/');
    }
    let ip: std::net::IpAddr = if authority.starts_with("[::1]") {
        std::net::Ipv6Addr::LOCALHOST.into()
    } else if authority.starts_with("127.0.0.1:") || authority.starts_with("localhost:") {
        std::net::Ipv4Addr::LOCALHOST.into()
    } else {
        return false;
    };
    let Ok(mut stream) = tokio::net::TcpStream::connect((ip, canvas.port)).await else {
        return false;
    };
    let request =
        format!("GET {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut head = Vec::new();
    let mut buf = [0u8; 256];
    while !head.windows(2).any(|w| w == b"\r\n") && head.len() < 1024 {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let Some(line_end) = head.windows(2).position(|w| w == b"\r\n") else {
        return false;
    };
    let Ok(line) = std::str::from_utf8(&head[..line_end]) else {
        return false;
    };
    status_ok(line)
}

/// `HTTP/1.x <code> …` with a code in 200..=399.
fn status_ok(line: &str) -> bool {
    let mut parts = line.split(' ');
    let version = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    let one_x = version
        .strip_prefix("HTTP/1.")
        .is_some_and(|m| m.len() == 1 && m.bytes().all(|b| b.is_ascii_digit()));
    one_x && code.len() == 3 && code.parse::<u16>().is_ok_and(|c| (200..=399).contains(&c))
}

/// A call id no other call of this process has used: a repeated id would join
/// the earlier execution.
fn fresh_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("home-{}-{n}-{nanos}", pid())
}

/// Send `payload` to `target` the way a page does: `user.forward` over
/// `ws://127.0.0.1:<port>/user/ws`, with no `Origin` header. The reply's data,
/// or why there is none.
pub async fn forward(
    port: u16,
    target: &str,
    payload: Value,
    timeout: Duration,
) -> Result<Value, String> {
    match tokio::time::timeout(timeout, forward_once(port, target, payload)).await {
        Ok(r) => r,
        Err(_) => Err("the fantastic did not answer".to_string()),
    }
}

async fn forward_once(port: u16, target: &str, payload: Value) -> Result<Value, String> {
    let url = format!("ws://127.0.0.1:{port}/user/ws");
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    let (mut ws, _) = tokio_tungstenite::client_async(url.as_str(), stream)
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    let id = fresh_id();
    let frame = json!({"type": "call", "target": "user", "payload": {"type": "forward", "target": target, "payload": payload}, "id": id});
    ws.send(Message::Text(frame.to_string()))
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    let outcome = loop {
        let Some(msg) = ws.next().await else {
            break Err(format!(
                "{url}: the connection closed before the fantastic answered"
            ));
        };
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => {
                break Err(format!(
                    "{url}: the connection closed before the fantastic answered"
                ))
            }
            Ok(_) => continue,
            Err(e) => break Err(format!("{url}: {e}")),
        };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if frame.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            continue;
        }
        match frame.get("type").and_then(Value::as_str) {
            Some("error") => {
                let error = frame
                    .get("error")
                    .and_then(Value::as_str)
                    .map_or_else(|| frame.to_string(), str::to_string);
                break Err(error);
            }
            Some("reply") => {
                let data = frame.get("data").cloned().unwrap_or(Value::Null);
                break match data.get("error").and_then(Value::as_str) {
                    Some(error) => Err(with_hint(error, &data)),
                    None => Ok(data),
                };
            }
            _ => continue,
        }
    };
    let _ = ws.close(None).await;
    outcome
}

/// `<error> — <hint>`, the hint only when the refusal carries one.
pub fn with_hint(error: &str, refusal: &Value) -> String {
    match refusal.get("hint").and_then(Value::as_str) {
        Some(hint) if !hint.is_empty() => format!("{error} — {hint}"),
        _ => error.to_string(),
    }
}

/// The guid of the kernel behind the canvas on `port`, from its `reflect`.
pub async fn guid(port: u16) -> Option<String> {
    let data = forward(
        port,
        "kernel",
        json!({"type": "reflect", "tree": "none"}),
        GUID_TIMEOUT,
    )
    .await
    .ok()?;
    data.get("guid")
        .and_then(Value::as_str)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
}

/// A folder's launch log name: `<last component>-<first 8 hex of sha256(path)>.log`.
pub fn log_file_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    let hash = Sha256::digest(path.as_bytes());
    let hex: String = hash.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{last}-{hex}.log")
}

/// Where launch logs go, on every OS: `<home>/logs`, created private as the
/// home store makes its dirs (`0700`; on Windows a DACL that protects only the user).
pub fn logs_dir(home: &Path) -> Result<PathBuf, String> {
    let dir = home.join("logs");
    fantastic_user_home_store::ensure_private_dir(home, &dir)?;
    Ok(dir)
}

/// Where the launch log of the fantastic in `path` goes.
pub fn log_path(home: &Path, path: &str) -> Result<PathBuf, String> {
    Ok(logs_dir(home)?.join(log_file_name(path)))
}

/// A spawn that failed says why in its launch log too: `error` is appended as a
/// line (the log is created `0600` when the spawn never got as far as opening
/// it). Returns `error`, so a failed spawn can pass it on.
pub fn record_spawn_failure(log: &Path, error: String) -> String {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // The log is a courtesy: when it cannot be written the error still travels.
    if let Ok(mut file) = options.open(log) {
        let _ = writeln!(file, "{error}");
    }
    error
}

/// The launch log of the fantastic in `path`, when it exists.
pub fn log_file(home: &Path, path: &str) -> Option<String> {
    let file = log_path(home, path).ok()?;
    file.is_file().then(|| file.display().to_string())
}

/// The last 4 lines of the launch log of the fantastic in `path`, cut to its
/// last 600 characters.
pub fn log_tail(home: &Path, path: &str) -> Option<String> {
    let bytes = std::fs::read(log_path(home, path).ok()?).ok()?;
    Some(tail_of(&String::from_utf8_lossy(&bytes)))
}

fn tail_of(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    let count = kept.chars().count();
    if count <= TAIL_CHARS {
        return kept;
    }
    kept.chars().skip(count - TAIL_CHARS).collect()
}
