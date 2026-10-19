//! THE STATE, as every client reads it: protocol `fantastic.home`, version 1.
//!
//! ```json
//! {"type":"state","protocol":"fantastic.home","version":1,"seq":7,"mode":"lock","listed":true,
//!  "rows":[{"id","guid","name","path","last_seen","status","canvas","pid","actions","log"}],
//!  "pages":["g-1"],"selection":"g-1","problem":null}
//! ```
//!
//! `status` is the `Status` serde name; `canvas` is `{url, port}` while running,
//! else `null`; `last_seen` is in seconds (`null` for a folder being added);
//! `log` is the launch log file when it exists.

use fantastic_home::{Home, Mode, Row, Status};
use serde_json::{json, Value};

pub const PROTOCOL: &str = "fantastic.home";
pub const VERSION: u32 = 1;

/// A status's serde name.
pub fn status_name(status: &Status) -> &'static str {
    match status {
        Status::Stopped => "stopped",
        Status::Starting => "starting",
        Status::Stopping => "stopping",
        Status::Running(_) => "running",
        Status::NoCanvas => "no_canvas",
        Status::Unverified => "unverified",
        Status::Missing => "missing",
    }
}

/// A mode's serde name.
pub fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Lock => "lock",
        Mode::Canvas => "canvas",
    }
}

fn row(r: &Row, mode: Mode, logs: &dyn Fn(&str) -> Option<String>) -> Value {
    let canvas = match &r.status {
        Status::Running(c) => json!({"url": c.url, "port": c.port}),
        _ => Value::Null,
    };
    json!({
        "id": r.id,
        "guid": r.guid,
        "name": r.name,
        "path": r.path,
        "last_seen": r.last_seen,
        "status": status_name(&r.status),
        "canvas": canvas,
        "pid": r.pid,
        "actions": r.actions(mode),
        "log": logs(&r.path),
    })
}

/// The state of `home` at `now_ms`, numbered `seq`. `logs` names a folder's
/// launch log file when it exists.
pub fn state(home: &Home, now_ms: u64, seq: u64, logs: &dyn Fn(&str) -> Option<String>) -> Value {
    let mode = home.config().mode;
    let rows: Vec<Value> = home
        .rows(now_ms)
        .iter()
        .map(|r| row(r, mode, logs))
        .collect();
    json!({
        "type": "state",
        "protocol": PROTOCOL,
        "version": VERSION,
        "seq": seq,
        "mode": mode_name(mode),
        "listed": home.listed(),
        "rows": rows,
        "pages": home.pages(),
        "selection": home.selection(),
        "problem": home.problem(),
    })
}
