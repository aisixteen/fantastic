//! PTY jobs — a terminal is a JOB of exec_bridge (terminal_backend merged in).
//!
//! Mirror of python `exec_bridge.tools`'s PTY section. `start pty={cols,rows}`
//! spawns through the same gate and the same spawner ([`crate::spawn::spawn_pty`]);
//! the job streams `output` events and is driven with `write` / `resize` / `ack` /
//! `output` / `paste_image`. terminal_backend's robustness, ported onto the job
//! table: incremental UTF-8 decode (a multi-byte char split across reads is not
//! `<?>` litter), writes in FULL and serialized per job (a bracketed paste is never
//! truncated or interleaved), a bounded scrollback RING for re-attach, and VSCode's
//! flow control (the reader pauses past [`HIGH_WATERMARK`] unacked chars; a
//! streamer's `ack` resumes it under [`LOW_WATERMARK`]).

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use fantastic_bundle::{py_repr, py_repr_value, py_str};
use fantastic_kernel::refusal::{bad, missing, refusal, with, NOT_FOUND, TRANSPORT_ERROR};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use tokio::io::unix::AsyncFd;

use crate::jobs;

/// The scrollback ring a re-attaching client replays (python `PTY_RING`).
pub const PTY_RING: usize = 256 * 1024;
/// Pause the reader past this many unacked chars (VSCode's constant).
pub const HIGH_WATERMARK: usize = 100_000;
/// Resume it once acks drain the backlog below this.
pub const LOW_WATERMARK: usize = 5_000;
/// A pasted image's size cap.
pub const MAX_PASTE_IMAGE: usize = 5 * 1024 * 1024;
/// The formats a CLI like Claude Code accepts, keyed by MIME.
const IMAGE_EXT: [(&str, &str); 4] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];

/// One PTY job's terminal: the master end and its mutable state.
pub struct Pty {
    fd: AsyncFd<OwnedFd>,
    st: Mutex<PtyInner>,
    /// Woken by `ack` when the backlog drains — the PUMP re-reads, in its own task.
    wake: tokio::sync::Notify,
    /// Serializes writes: two interleaved `write_all`s would corrupt a bracketed paste.
    write_lock: tokio::sync::Mutex<()>,
}

struct PtyInner {
    cols: u16,
    rows: u16,
    ring: VecDeque<String>,
    ring_bytes: usize,
    unacked: usize,
    paused: bool,
    eof: bool,
    decoder: encoding_rs::Decoder,
    paste_dir: Option<PathBuf>,
}

impl Pty {
    /// Wrap a fresh master fd (already non-blocking) at the size it was opened with.
    pub fn new(master: OwnedFd, cols: u16, rows: u16) -> std::io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            fd: AsyncFd::new(master)?,
            st: Mutex::new(PtyInner {
                cols,
                rows,
                ring: VecDeque::new(),
                ring_bytes: 0,
                unacked: 0,
                paused: false,
                eof: false,
                decoder: encoding_rs::UTF_8.new_decoder_without_bom_handling(),
                paste_dir: None,
            }),
            wake: tokio::sync::Notify::new(),
            write_lock: tokio::sync::Mutex::new(()),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, PtyInner> {
        self.st.lock().expect("pty poisoned")
    }

    /// `(cols, rows)` — for `status`.
    pub fn dims(&self) -> (u16, u16) {
        let g = self.lock();
        (g.cols, g.rows)
    }
}

/// Set the window size on the master (the tty sends SIGWINCH to the program).
pub fn set_winsize(fd: i32, cols: u16, rows: u16) -> std::io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: a valid winsize on an fd we own.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// THE JOB'S TASK for a PTY job (python `_pty_pump`). It owns the reader — reading
/// HERE, never in a verb's dispatch, keeps every `output` emit in the job's own
/// context — pauses it past the high-water mark until an `ack` wakes it, and ends
/// at EOF with `job_done`.
pub async fn pump(
    owner: AgentId,
    ready: jobs::Ready,
    mut child: tokio::process::Child,
    pty: Arc<Pty>,
    kernel: Arc<Kernel>,
) {
    // The job's id is this task's own id, sent once `start` has filed the row.
    let Some(job_id) = jobs::await_id(ready, &child).await else {
        return;
    };
    let mut finalize = jobs::FinalizeRow::new((owner.clone(), job_id));
    let mut buf = vec![0u8; 4096];
    loop {
        let paused = {
            let g = pty.lock();
            if g.eof {
                break;
            }
            g.paused
        };
        if paused {
            pty.wake.notified().await;
            continue;
        }
        let Ok(mut guard) = pty.fd.readable().await else {
            pty.lock().eof = true;
            break;
        };
        let read = guard.try_io(|inner| {
            // SAFETY: a valid buffer on an fd we own.
            let n = unsafe { libc::read(inner.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(usize::try_from(n).unwrap_or(0))
            }
        });
        let n = match read {
            Err(_would_block) => continue,
            // EOF, or EIO once the child closed the slave: the program is done.
            Ok(Ok(0) | Err(_)) => {
                pty.lock().eof = true;
                break;
            }
            Ok(Ok(n)) => n,
        };
        let text = {
            let mut g = pty.lock();
            let mut out =
                String::with_capacity(g.decoder.max_utf8_buffer_length(n).unwrap_or(n * 3 + 4));
            let _ = g.decoder.decode_to_string(&buf[..n], &mut out, false);
            if out.is_empty() {
                continue;
            }
            ring_add(&mut g, &out);
            g.unacked += out.chars().count();
            if g.unacked > HIGH_WATERMARK {
                g.paused = true;
            }
            out
        };
        kernel
            .emit(
                &owner,
                json!({"type": "output", "job_id": job_id.get(), "data": text}),
            )
            .await;
    }
    let code = child.wait().await.ok().and_then(jobs::exit_code);
    finalize.disarm();
    {
        let mut g = jobs::jobs();
        if let Some(j) = g.get_mut(&(owner.clone(), job_id)) {
            j.exit_code = code;
            j.status = if j.status == "stopping" {
                "stopped".to_string()
            } else if code == Some(0) {
                "done".to_string()
            } else {
                "failed".to_string()
            };
        }
    }
    if let Some(dir) = pty.lock().paste_dir.take() {
        let _ = fantastic_file::fs::remove(&dir, "", true);
    }
    kernel
        .emit(
            &owner,
            json!({"type": "job_done", "job_id": job_id.get(), "exit_code": code,
                   "ok": code == Some(0)}),
        )
        .await;
}

fn ring_add(g: &mut PtyInner, text: &str) {
    g.ring.push_back(text.to_string());
    g.ring_bytes += text.len();
    while g.ring_bytes > PTY_RING {
        match g.ring.pop_front() {
            Some(old) => g.ring_bytes -= old.len(),
            None => break,
        }
    }
}

/// Write the FULL buffer to the non-blocking master: `write` short-writes and
/// fails `EAGAIN` when the tty buffer is full; one unchecked write drops the tail
/// — a bracketed paste then loses its end marker and the shell sticks in paste
/// mode. Loop until every byte lands.
async fn write_all(pty: &Pty, data: &[u8]) -> std::io::Result<usize> {
    let mut written = 0;
    while written < data.len() {
        let mut guard = pty.fd.writable().await?;
        match guard.try_io(|inner| {
            let rest = &data[written..];
            // SAFETY: a valid buffer on an fd we own.
            let n = unsafe { libc::write(inner.as_raw_fd(), rest.as_ptr().cast(), rest.len()) };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(usize::try_from(n).unwrap_or(0))
            }
        }) {
            Ok(Ok(n)) => written += n,
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => {}
        }
    }
    Ok(written)
}

/// `(pty, None)` for a live PTY job this agent owns, or the refusal (python
/// `_pty_job`).
fn pty_job(verb: &str, owner: &AgentId, payload: &Value) -> Result<Arc<Pty>, Value> {
    let jid = crate::require_job(verb, payload)?;
    let g = jobs::jobs();
    let Some(job) = jid.find(&g, owner) else {
        return Err(crate::no_such_job(&jid));
    };
    let Some(pty) = job.pty.clone() else {
        return Err(bad(verb, "job_id", "a PTY job (started with `pty`)"));
    };
    if !jobs::alive(job) || pty.lock().eof {
        let mut v = refusal(
            NOT_FOUND,
            format!("exec_bridge: job {} has ended", py_repr_value(&jid.raw)),
        );
        with(&mut v, "job_id", jid.raw);
        return Err(v);
    }
    Ok(pty)
}

/// A positive `u16` (python `_dims`: an int >= 1, a bool is not one).
fn dim(verb: &str, name: &str, v: Option<&Value>) -> Result<u16, Value> {
    v.and_then(Value::as_u64)
        .filter(|n| *n >= 1)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| bad(verb, name, "a positive integer"))
}

/// `(cols, rows)` of a `pty` / `resize` object — both required, positive.
pub fn dims(verb: &str, raw: &Value) -> Result<(u16, u16), Value> {
    if !raw.is_object() {
        return Err(bad(verb, "pty", "an object {cols, rows}"));
    }
    Ok((
        dim(verb, "cols", raw.get("cols"))?,
        dim(verb, "rows", raw.get("rows"))?,
    ))
}

/// `write {job_id, data}` — keystrokes / paste.
pub async fn write_reply(owner: &AgentId, payload: &Value) -> Value {
    let pty = match pty_job("write", owner, payload) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(data) = payload.get("data").and_then(Value::as_str) else {
        return bad("write", "data", "a string");
    };
    let _guard = pty.write_lock.lock().await;
    match write_all(&pty, data.as_bytes()).await {
        Ok(n) => json!({"written": n}),
        Err(e) => refusal(TRANSPORT_ERROR, format!("exec_bridge: write failed: {e}")),
    }
}

/// `resize {job_id, cols, rows}`.
pub fn resize_reply(owner: &AgentId, payload: &Value) -> Value {
    let pty = match pty_job("resize", owner, payload) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let (cols, rows) = match dims("resize", payload) {
        Ok(d) => d,
        Err(e) => return e,
    };
    {
        let mut g = pty.lock();
        g.cols = cols;
        g.rows = rows;
    }
    if let Err(e) = set_winsize(pty.fd.get_ref().as_raw_fd(), cols, rows) {
        return refusal(TRANSPORT_ERROR, format!("exec_bridge: resize failed: {e}"));
    }
    json!({"resized": true, "cols": cols, "rows": rows})
}

/// `ack {job_id, chars}` — flow control.
pub fn ack_reply(owner: &AgentId, payload: &Value) -> Value {
    let pty = match pty_job("ack", owner, payload) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(chars) = payload.get("chars").and_then(Value::as_u64) else {
        return bad("ack", "chars", "a non-negative integer");
    };
    let (unacked, paused) = {
        let mut g = pty.lock();
        g.unacked = g
            .unacked
            .saturating_sub(usize::try_from(chars).unwrap_or(usize::MAX));
        if g.paused && g.unacked < LOW_WATERMARK {
            g.paused = false;
            // the pump re-reads in the job's own context
            pty.wake.notify_one();
        }
        (g.unacked, g.paused)
    };
    json!({"unacked": unacked, "paused": paused})
}

/// `output {job_id, max_bytes?}` — the ring, for a client re-attaching.
pub fn output_reply(owner: &AgentId, payload: &Value) -> Value {
    let jid = match crate::require_job("output", payload) {
        Ok(j) => j,
        Err(e) => return e,
    };
    let g = jobs::jobs();
    let Some(job) = jid.find(&g, owner) else {
        return crate::no_such_job(&jid);
    };
    let Some(pty) = job.pty.clone() else {
        return bad(
            "output",
            "job_id",
            "a PTY job (a pipe job's output is in `status`)",
        );
    };
    drop(g);
    let mut text: String = pty.lock().ring.iter().map(String::as_str).collect();
    if let Some(mb) = payload.get("max_bytes").filter(|v| !v.is_null()) {
        let Some(mb) = mb.as_u64().and_then(|n| usize::try_from(n).ok()) else {
            return bad("output", "max_bytes", "a non-negative integer");
        };
        if mb < text.len() {
            // python slices the BYTES and decodes with errors="ignore": a char cut
            // at the front is dropped, never shown as litter.
            let mut start = text.len() - mb;
            while start < text.len() && !text.is_char_boundary(start) {
                start += 1;
            }
            text = text[start..].to_string();
        }
    }
    let shown = py_str(&jid.raw);
    json!({
        "job_id": jid.raw,
        "output": text,
        "origin": format!("{owner} output {{job_id: {shown}}} — the ring NOW; re-send, it moves"),
    })
}

/// `paste_image` on the TEXT channel: there are no bytes to paste — the job is
/// checked first, as python's handler does, then `data` refuses.
pub fn paste_image_text(owner: &AgentId, payload: &Value) -> Value {
    match pty_job("paste_image", owner, payload) {
        Ok(_) => bad("paste_image", "data", "raw bytes"),
        Err(e) => e,
    }
}

/// `paste_image {job_id, data:bytes, mime}` — the BINARY channel: `data` IS the blob.
pub async fn paste_image_reply(owner: &AgentId, header: &Value, data: &[u8]) -> Value {
    let pty = match pty_job("paste_image", owner, header) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if data.len() > MAX_PASTE_IMAGE {
        return bad(
            "paste_image",
            "data",
            &format!("at most {MAX_PASTE_IMAGE} bytes"),
        );
    }
    // The mime is the caller's to state — never guessed.
    let mime = match header.get("mime").and_then(Value::as_str) {
        Some(m) if !m.is_empty() => m.to_lowercase(),
        _ => return missing("paste_image", "mime"),
    };
    let Some((_, ext)) = IMAGE_EXT.iter().find(|(m, _)| *m == mime) else {
        let mut legal: Vec<&str> = IMAGE_EXT.iter().map(|(m, _)| *m).collect();
        legal.sort_unstable();
        return bad(
            "paste_image",
            "mime",
            &format!(
                "one of [{}]",
                legal
                    .iter()
                    .map(|m| py_repr(m))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    };
    let dir = {
        let existing = pty.lock().paste_dir.clone();
        match existing {
            Some(d) => d,
            None => match mkdtemp(&format!("fantastic_paste_{owner}_")) {
                Ok(d) => {
                    pty.lock().paste_dir = Some(d.clone());
                    d
                }
                Err(e) => {
                    return refusal(
                        TRANSPORT_ERROR,
                        format!("exec_bridge: paste_image failed: {e}"),
                    )
                }
            },
        }
    };
    let name = format!("paste_{}.{ext}", crate::rand_hex8());
    // The scratch dir is a system temp dir (outside cwd by design) — written
    // through `fs`, the one disk module.
    if let Err(e) = fantastic_file::fs::write_bytes(&dir, &name, data, false, None) {
        return refusal(
            TRANSPORT_ERROR,
            format!("exec_bridge: paste_image failed: {e}"),
        );
    }
    let path = dir.join(&name).to_string_lossy().to_string();
    let _guard = pty.write_lock.lock().await;
    // the path at the cursor with a trailing space, no newline — pasting must not submit
    if let Err(e) = write_all(&pty, format!("{path} ").as_bytes()).await {
        return refusal(
            TRANSPORT_ERROR,
            format!("exec_bridge: paste_image failed: {e}"),
        );
    }
    json!({"path": path, "bytes": data.len()})
}

/// python `tempfile.mkdtemp(prefix=…)` — a 0700 dir in the system temp dir.
fn mkdtemp(prefix: &str) -> std::io::Result<PathBuf> {
    let template = std::env::temp_dir().join(format!("{prefix}XXXXXXXX"));
    let mut bytes = template.to_string_lossy().into_owned().into_bytes();
    bytes.push(0);
    // SAFETY: a NUL-terminated, writable template buffer.
    let ptr = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
    if ptr.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    bytes.pop();
    Ok(PathBuf::from(String::from_utf8_lossy(&bytes).to_string()))
}
