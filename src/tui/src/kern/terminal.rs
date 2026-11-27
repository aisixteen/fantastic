//! THE IN-APP TERMINAL, as an agent. `t` in the tree opens it and `t` again
//! closes it.
//!
//! It is an `exec_bridge` agent `term` under `user`, which no blueprint seeds,
//! running ONE PTY job: the human's `$SHELL`. It sits in the tree like everything
//! else, and a coder run inside it reaches the kernel the same way any other
//! client does.
//!
//! Opening sets its `allowed` to exactly the line it runs (it runs in the project
//! dir and inherits this process's environment): `t` makes `term` with
//! `create_agent`. The shell drives it as the operator. Attaching is
//! CLIENT-SIDE, because `boot` starts nothing: `status` finds a live PTY job and
//! re-attaches to it (its `output` ring is the replay), or `start`s the shell.
//! Closing is a `delete_agent`; the job dies with it.
//!
//! `fantastic run COMMAND [ARGS…]` (`recipe.rs`): `term` runs exactly that argv
//! instead of the shell — the user's own program from `$PATH`, as is, at their
//! discretion. The argv is `term`'s one allowed line (read back by `recorded`), so a re-attach starts
//! the same program; a different `run` remakes `term`. With the canvas the same
//! program runs in a terminal CELL instead ([`canvas_cell`]).

use serde_json::{json, Value};

use crate::kern::cps::Done;
use crate::kern::host::Host;

/// The terminal's agent id.
pub const TERM_ID: &str = "term";

/// The PTY job a pane draws, and what it printed before the pane attached.
#[derive(Debug, Clone, PartialEq)]
pub struct Attach {
    /// The job's kernel task id — an INTEGER, as `start`/`status`/`ps` report it.
    pub job_id: i64,
    pub replay: String,
}

/// The shell the terminal runs. `$SHELL` (`%COMSPEC%` on Windows, `sys`), and
/// nothing is guessed when it is unset.
fn shell() -> Result<String, String> {
    let var = crate::sys::SHELL_VAR;
    match std::env::var(var) {
        Ok(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(format!("{var} is not set — the terminal runs ${var} and guesses no other")),
    }
}

/// The `allowed` pattern that admits exactly `line` (the joined argv) and no
/// other command.
fn exactly(line: &str) -> String {
    regex::escape(line)
}

/// The exec record fields that run `argv` and nothing else: the exact line
/// allowed, in the directory the user ran us in (the process cwd), with the
/// human's whole environment — the human's own program gets the human's env.
fn line_record(argv: &[String]) -> Value {
    let line = fantastic_exec_bridge::shlex_join(argv);
    json!({"allowed": [exactly(&line)], "cwd": ".", "inherit_env": ["*"]})
}

/// Undo [`exactly`]: the line a `regex::escape`d pattern admits (every `\`
/// escapes the char after it).
fn unescape(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len());
    let mut it = pat.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Split a line `shlex_join` made back into its argv: tokens split on blanks,
/// `'…'` literal, `"…"` with `\` escaping `"` and `\`. `None` on an unclosed quote.
fn shlex_split(line: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut live = false;
    let mut it = line.chars();
    while let Some(c) = it.next() {
        match c {
            '\'' => {
                live = true;
                loop {
                    match it.next()? {
                        '\'' => break,
                        ch => cur.push(ch),
                    }
                }
            }
            '"' => {
                live = true;
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => cur.push(it.next()?),
                        ch => cur.push(ch),
                    }
                }
            }
            c if c.is_whitespace() => {
                if live {
                    out.push(std::mem::take(&mut cur));
                    live = false;
                }
            }
            ch => {
                live = true;
                cur.push(ch);
            }
        }
    }
    if live {
        out.push(cur);
    }
    Some(out)
}

/// The argv `term`'s record names (`run`), if any: its ONE allowed line
/// ([`line_record`]), unwrapped and split.
fn recorded(host: &Host, id: &str) -> Option<Vec<String>> {
    let a = host.kernel.resolve_target(id)?;
    let meta = a.record().meta.clone();
    argv_of(meta.get("allowed")?)
}

/// The argv a record's `allowed` list names: exactly one pattern, an
/// [`exactly`] line.
fn argv_of(allowed: &Value) -> Option<Vec<String>> {
    let [pat] = allowed.as_array()?.as_slice() else { return None };
    shlex_split(&unescape(pat.as_str()?))
}

/// `run COMMAND…` in the terminal UI: `term` will run exactly `argv`. A `term`
/// running anything else is removed first (its job dies with it); the pane
/// attaches as for `t` ([`open`] starts the recorded argv). `done` is told when the
/// last call settled.
pub async fn prepare_run(host: &Host, argv: &[String], done: Done<Result<(), String>>) {
    let id = fantastic_kernel::AgentId::from(TERM_ID);
    if host.kernel.exists(id.as_str()) {
        if recorded(host, TERM_ID).as_deref() == Some(argv) {
            return done(Ok(())).await;
        }
        let (h, argv) = (host.clone(), argv.to_vec());
        return close(
            host,
            crate::kern::cps::done(move |r: Result<(), String>| async move {
                match r {
                    Ok(()) => create(&h, &argv, done).await,
                    Err(e) => done(Err(e)).await,
                }
            }),
        )
        .await;
    }
    create(host, argv, done).await
}

/// The `create_agent` of an exec agent under `parent` running exactly `argv`.
fn exec_spec(parent: &str, id: &str, argv: &[String]) -> Value {
    let mut create = json!({
        "type": "create_agent",
        "parent_id": parent,
        "id": id,
        "handler_module": fantastic_exec_bridge::HANDLER_MODULE,
    });
    if let (Some(c), Value::Object(f)) = (create.as_object_mut(), line_record(argv)) {
        c.extend(f);
    }
    create
}

/// Create `term` under `user`, running exactly `argv`. It holds no lines.
async fn create(host: &Host, argv: &[String], done: Done<Result<(), String>>) {
    let create = exec_spec(host.cfg.user.as_str(), TERM_ID, argv);
    host.operator_then("kernel", create, move |out| async move {
        done(match out.result.get("error").and_then(Value::as_str) {
            Some(e) => Err(format!("could not create `{TERM_ID}`: {e}")),
            None => Ok(()),
        })
        .await;
    })
    .await;
}

/// `run COMMAND…` with the canvas: an exec agent under `canvas_root` that may
/// run exactly `argv`, and a terminal cell on the view that starts it (the
/// cell's `command`). `done` gets the cell's id; `None` when the page has no view yet.
/// Each step is sent when the one before settled.
pub async fn canvas_cell(host: &Host, argv: &[String], done: Done<Result<Option<String>, String>>) {
    let (h, argv) = (host.clone(), argv.to_vec());
    crate::kern::root_ai::has_view(
        host,
        crate::kern::cps::done(move |r: Result<bool, String>| async move {
            match r {
                Err(e) => return done(Err(e)).await,
                Ok(false) => return done(Ok(None)).await,
                Ok(true) => {}
            }
            let hex = crate::kern::backend::hex8();
            let (exec, cell) = (format!("run_{hex}"), format!("terminal_{hex}"));
            let create = exec_spec(crate::kern::web::CANVAS_NODE_ID, &exec, &argv);
            let h2 = h.clone();
            h.operator_then("kernel", create, move |out| async move {
                if let Some(e) = out.result.get("error").and_then(Value::as_str) {
                    return done(Err(format!("could not create `{exec}`: {e}"))).await;
                }
                let mut rec = serde_json::Map::new();
                for (k, v) in [
                    ("id", json!(cell)),
                    ("handler_module", json!("terminal_view.ts")),
                    ("parent_id", json!("view_root")),
                    ("owns_backend", json!(true)),
                    ("backend_id", json!(exec)),
                    ("command", json!(argv)),
                    ("display_name", json!(argv.join(" "))),
                    ("x", json!(0)),
                    ("y", json!(0)),
                    ("width", json!(720)),
                    ("height", json!(420)),
                    ("can", json!({exec.clone(): {"call": ["watch", "status", "start", "write", "ack", "resize", "output", "paste_image"]}})),
                ] {
                    rec.insert(k.into(), v);
                }
                crate::kern::root_ai::persist(
                    &h2,
                    rec,
                    crate::kern::cps::done(move |r: Result<(), String>| async move {
                        done(r.map(|()| Some(cell))).await;
                    }),
                )
                .await;
            })
            .await;
        }),
    )
    .await;
}

/// Make `term` (created running the human's shell when it is missing), then
/// attach to its live PTY job or start one at `cols`×`rows`; `done` gets what the pane
/// draws. The chain: `create` when missing, `status`, then `output` (a live job is
/// re-attached) or `start`.
pub async fn open(host: &Host, cols: u16, rows: u16, done: Done<Result<Attach, String>>) {
    if host.kernel.exists(TERM_ID) {
        return opened(host.clone(), cols, rows, done).await;
    }
    let argv = match shell() {
        Ok(sh) => vec![sh],
        Err(e) => return done(Err(e)).await,
    };
    let h = host.clone();
    create(
        host,
        &argv,
        crate::kern::cps::done(move |r: Result<(), String>| async move {
            match r {
                Ok(()) => opened(h, cols, rows, done).await,
                Err(e) => done(Err(e)).await,
            }
        }),
    )
    .await;
}

/// `term` exists: read its state, and attach or start.
async fn opened(host: Host, cols: u16, rows: u16, done: Done<Result<Attach, String>>) {
    // the program: the argv its one line names (the shell, or what `run` gave)
    let Some(argv) = recorded(&host, TERM_ID).filter(|a| !a.is_empty()) else {
        return done(Err(format!("`{TERM_ID}` names no one allowed line to run"))).await;
    };
    // A live PTY job is re-attached, never doubled.
    let h = host.clone();
    host.operator_then(TERM_ID, json!({"type": "status"}), move |st| async move {
        if let Some(e) = st.result.get("error").and_then(Value::as_str) {
            return done(Err(format!("`{TERM_ID}` status: {e}"))).await;
        }
        if let Some(job_id) = live_pty(&st.result) {
            return h
                .operator_then(TERM_ID, json!({"type": "output", "job_id": job_id}), move |r| async move {
                    if let Some(e) = r.result.get("error").and_then(Value::as_str) {
                        return done(Err(format!("`{TERM_ID}` output: {e}"))).await;
                    }
                    let replay = r.result.get("output").and_then(Value::as_str).unwrap_or_default().to_string();
                    done(Ok(Attach { job_id, replay })).await;
                })
                .await;
        }
        let start = json!({"type": "start", "command": argv[0], "args": &argv[1..], "pty": {"cols": cols, "rows": rows}});
        // the start settles when the job exits; the job is learned from `term`'s own
        // announcement (an inbox watch), never from the settle
        done(match started(&h, start).await {
            Ok(job_id) => Ok(Attach { job_id, replay: String::new() }),
            Err(r) => {
                let e = r.get("error").and_then(Value::as_str);
                let why = r.get("reason").and_then(Value::as_str).or(e);
                match why {
                    Some(why) => Err(format!("`{TERM_ID}` did not start {}: {why}", argv.join(" "))),
                    None => Err(format!("`{TERM_ID}` start named no job: {r}")),
                }
            }
        })
        .await;
    })
    .await;
}

/// `start` the PTY job. Its call SETTLES WHEN THE JOB EXITS (#795), so the pane learns
/// the job from `term`'s `job_started` announcement
/// ([`fantastic_exec_bridge::start_announced`]); the shell's end comes to the pane as
/// `job_done`. A start that settles before any announcement (refused) is the `Err`.
async fn started(host: &Host, start: Value) -> Result<i64, Value> {
    let id = fantastic_kernel::AgentId::from(TERM_ID);
    let ann = fantastic_exec_bridge::start_announced(&host.kernel, &id, start).await?;
    ann.get("job_id").and_then(Value::as_i64).ok_or(ann)
}

/// The first running PTY job in a `status` reply.
fn live_pty(status: &Value) -> Option<i64> {
    status.get("jobs")?.as_array()?.iter().find_map(|j| {
        let live = j.get("pty").and_then(Value::as_bool) == Some(true)
            && j.get("status").and_then(Value::as_str) == Some("running");
        live.then(|| j.get("job_id").and_then(Value::as_i64)).flatten()
    })
}

/// Delete `term`; its PTY job dies with it. The next `t` makes it again.
pub async fn close(host: &Host, done: Done<Result<(), String>>) {
    host.operator_then("kernel", json!({"type": "delete_agent", "id": TERM_ID}), move |out| async move {
        done(match out.result.get("error").and_then(Value::as_str) {
            Some(e) => Err(format!("could not close `{TERM_ID}`: {e}")),
            None => Ok(()),
        })
        .await;
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `close` is a call: its refusal (no such agent) is the answer it gives, from the settle.
    #[tokio::test]
    async fn close_answers_from_its_call() {
        let (h, dir) = crate::kern::backend::create_tests::host("term-close").await;
        let r = crate::kern::cps::run(|d| close(&h, d)).await;
        assert!(r.is_err(), "no `term` here to close: {r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_running_pty_job_is_reattached() {
        let st = json!({"jobs": [
            {"job_id": 1, "pty": false, "status": "running"},
            {"job_id": 2, "pty": true, "status": "done"},
            {"job_id": 3, "pty": true, "status": "running"},
        ]});
        assert_eq!(live_pty(&st), Some(3));
        assert_eq!(live_pty(&json!({"jobs": []})), None);
    }

    #[test]
    fn the_allowlist_admits_the_shell_line_and_nothing_else() {
        let line = fantastic_exec_bridge::shlex_join(&["/bin/zsh".to_string()]);
        let re = regex::Regex::new(&format!("^(?:{})$", exactly(&line))).unwrap();
        assert!(re.is_match("/bin/zsh"));
        assert!(!re.is_match("/bin/zsh -c id"));
        assert!(!re.is_match("/bin/zsha"));
    }

    #[test]
    fn the_recorded_argv_is_read_back_from_the_allowed_line() {
        for argv in [
            vec!["/bin/zsh".to_string()],
            vec!["echo".into(), "hi-42".into()],
            vec!["sh".into(), "-c".into(), "a b|c (d) [e] $x.y+z".into()],
            vec!["x".into(), "".into(), "it's".into()],
        ] {
            let rec = line_record(&argv);
            assert_eq!(argv_of(&rec["allowed"]), Some(argv.clone()), "{rec}");
        }
        assert_eq!(argv_of(&json!([])), None);
        assert_eq!(argv_of(&json!(["a", "b"])), None);
        assert_eq!(shlex_split("a 'b"), None);
    }
}
