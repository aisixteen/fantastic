//! `tail` / `grep` — a job's output SO FAR. A pipe job's
//! stdout and stderr land in `status` only when it ends; while it runs, a caller
//! reads the live line ring: the last lines, or the lines matching a regex, each
//! `{n, stream, line}`. A PTY job's one stream is its ring, split into lines.
//! Python `exec_bridge.tools._tail` / `_grep`.

use crate::jobs::{self, Job};
use fancy_regex::Regex;
use fantastic_bundle::{py_repr, py_splitlines, py_str};
use fantastic_kernel::refusal::bad;
use fantastic_kernel::AgentId;
use serde_json::{json, Value};

/// A pipe job's live line ring, in bytes — the same bound as the PTY ring.
pub const LINE_RING: usize = 256 * 1024;
/// The most lines one `tail` / `grep` reply carries.
pub const MAX_LINES: i64 = 500;

/// Append one printed line to a pipe job's ring, dropping the oldest past
/// [`LINE_RING`]. Python `_line_ring_add`.
pub fn line_ring_add(job: &mut Job, n: u64, stream: &str, line: &str) {
    job.line_ring
        .push_back((n, stream.to_string(), line.to_string()));
    job.line_ring_bytes += line.len() + 1;
    while job.line_ring_bytes > LINE_RING {
        let Some((_, _, old)) = job.line_ring.pop_front() else {
            break;
        };
        job.line_ring_bytes -= old.len() + 1;
        job.line_ring_dropped += 1;
    }
}

/// The output a job has printed so far, oldest first, and how many earlier lines
/// the ring already dropped. Python `_job_lines`.
fn job_lines(job: &Job, stream: &str) -> (Vec<(u64, String, String)>, u64) {
    if let Some(pty) = &job.pty {
        let text = pty.ring_text();
        let rows = py_splitlines(&text)
            .into_iter()
            .enumerate()
            .map(|(i, l)| ((i + 1) as u64, "pty".to_string(), l.to_string()))
            .collect();
        return (rows, 0);
    }
    let rows = job
        .line_ring
        .iter()
        .filter(|(_, s, _)| stream == "both" || s == stream)
        .cloned()
        .collect();
    (rows, job.line_ring_dropped)
}

fn lines_arg(verb: &str, payload: &Value, default: i64) -> Result<usize, Value> {
    match payload.get("lines") {
        None => Ok(default as usize),
        Some(Value::Number(n))
            if n.is_i64() && (1..=MAX_LINES).contains(&n.as_i64().unwrap_or(0)) =>
        {
            Ok(n.as_i64().unwrap_or(default) as usize)
        }
        Some(_) => Err(bad(verb, "lines", &format!("an integer 1–{MAX_LINES}"))),
    }
}

fn stream_arg(verb: &str, payload: &Value) -> Result<String, Value> {
    match payload.get("stream") {
        None => Ok("both".to_string()),
        Some(Value::String(s)) if matches!(s.as_str(), "stdout" | "stderr" | "both") => {
            Ok(s.clone())
        }
        Some(_) => Err(bad(verb, "stream", "one of stdout, stderr, both")),
    }
}

fn row(r: &(u64, String, String)) -> Value {
    json!({"n": r.0, "stream": r.1, "line": r.2})
}

/// `tail {job_id, lines?, stream?}` — the last lines a job has printed so far.
pub fn tail_reply(owner: &AgentId, payload: &Value) -> Value {
    let jid = match crate::require_job("tail", payload) {
        Ok(j) => j,
        Err(e) => return e,
    };
    let n = match lines_arg("tail", payload, 20) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let stream = match stream_arg("tail", payload) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let g = jobs::jobs();
    let Some(job) = jid.find(&g, owner) else {
        return crate::no_such_job(&jid);
    };
    let (rows, dropped) = job_lines(job, &stream);
    let printed = if job.pty.is_some() {
        rows.len() as u64
    } else {
        job.lines
    };
    let shown = py_str(&jid.raw);
    json!({
        "job_id": jid.raw,
        "status": job.status,
        "lines": rows[rows.len().saturating_sub(n)..].iter().map(row).collect::<Vec<_>>(),
        "printed": printed,
        "dropped": dropped,
        "origin": format!("{owner} tail {{job_id: {shown}}} — the output NOW; re-send, it moves"),
    })
}

/// `grep {job_id, match, lines?, stream?}` — the lines printed so far matching a
/// regex (the last `lines` of them).
pub fn grep_reply(owner: &AgentId, payload: &Value) -> Value {
    let jid = match crate::require_job("grep", payload) {
        Ok(j) => j,
        Err(e) => return e,
    };
    let pat = match payload.get("match") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => return bad("grep", "match", "a regex string"),
    };
    let rx = match Regex::new(&pat) {
        Ok(r) => r,
        Err(e) => return bad("grep", "match", &format!("a valid regex ({e})")),
    };
    let n = match lines_arg("grep", payload, 50) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let stream = match stream_arg("grep", payload) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let g = jobs::jobs();
    let Some(job) = jid.find(&g, owner) else {
        return crate::no_such_job(&jid);
    };
    let (rows, dropped) = job_lines(job, &stream);
    let hits: Vec<&(u64, String, String)> = rows
        .iter()
        .filter(|r| rx.is_match(&r.2).unwrap_or(false))
        .collect();
    let shown = py_str(&jid.raw);
    json!({
        "job_id": jid.raw,
        "status": job.status,
        "matches": hits[hits.len().saturating_sub(n)..].iter().map(|r| row(r)).collect::<Vec<_>>(),
        "total": hits.len(),
        "scanned": rows.len(),
        "dropped": dropped,
        "origin": format!(
            "{owner} grep {{job_id: {shown}, match: {}}} — the output NOW; re-send, it moves",
            py_repr(&pat)
        ),
    })
}
