//! THE IN-APP TERMINAL, as an agent. `t` in the tree opens it and `t` again
//! closes it.
//!
//! It is an `exec_bridge` agent `term` under `user`, running ONE PTY
//! job: the human's `$SHELL`. It sits in the tree like everything else, and a
//! coder run inside it reaches the kernel the same way any other client does.
//!
//! Opening is a `create_agent` (its record allows exactly the shell line, runs
//! in the project dir, inherits this process's environment), then a MERGE of the
//! new id into the human's `can_call` (so `user → term` passes the gate).
//! Attaching is CLIENT-SIDE, because `boot` starts nothing: `status` finds a live
//! PTY job and re-attaches to it (its `output` ring is the replay), or `start`s
//! the shell as the human. Closing is a `delete_agent`; the job dies with it.

use serde_json::{json, Value};

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

/// The shell the terminal runs. `$SHELL`, and nothing is guessed when it is unset.
fn shell() -> Result<String, String> {
    match std::env::var("SHELL") {
        Ok(s) if !s.trim().is_empty() => Ok(s),
        _ => Err("SHELL is not set — the terminal runs $SHELL and guesses no other".into()),
    }
}

/// The `allowed` pattern that admits exactly `line` (the joined argv) and no
/// other command.
fn exactly(line: &str) -> String {
    regex::escape(line)
}

/// The exec record fields that run `sh` and nothing else: the exact line
/// allowed, in the directory the user ran us in (the process cwd), with the
/// human's whole environment — the human's own shell gets the human's env.
fn shell_record(sh: &str) -> Value {
    let line = fantastic_exec_bridge::shlex_join(&[sh.to_string()]);
    json!({"allowed": [exactly(&line)], "cwd": ".", "inherit_env": ["*"]})
}

/// Create `term` if it is missing, then attach to its live PTY job or start one
/// at `cols`×`rows`.
pub async fn open(host: &Host, cols: u16, rows: u16) -> Result<Attach, String> {
    let sh = shell()?;
    let id = fantastic_kernel::AgentId::from(TERM_ID);
    if !host.kernel.agents.contains_key(&id) {
        let mut create = json!({
            "type": "create_agent",
            "parent_id": host.cfg.user.as_str(),
            "id": TERM_ID,
            "handler_module": fantastic_exec_bridge::HANDLER_MODULE,
            "ingress_rule": "allow_all_ingress",
            "callable_by": {host.cfg.user.as_str(): "*"},
        });
        if let (Some(c), Value::Object(f)) = (create.as_object_mut(), shell_record(&sh)) {
            c.extend(f);
        }
        let r = host.operator("kernel", create).await;
        if let Some(e) = r.result.get("error").and_then(Value::as_str) {
            return Err(format!("could not create `{TERM_ID}`: {e}"));
        }
        // The human's authority must name the terminal, or every keystroke is
        // refused. MERGED into what is there, never replacing it.
        let user = host.cfg.user.clone();
        let mut cc = host
            .kernel
            .agents
            .get(&fantastic_kernel::AgentId::from(user.as_str()))
            .and_then(|a| a.value().record().meta.get("can_call").and_then(Value::as_object).cloned())
            .unwrap_or_default();
        cc.insert(TERM_ID.into(), json!("*"));
        let r = host
            .operator("kernel", json!({"type": "update_agent", "id": user, "can_call": cc}))
            .await;
        if let Some(e) = r.result.get("error").and_then(Value::as_str) {
            return Err(format!("could not grant `{user}` → `{TERM_ID}`: {e}"));
        }
    }

    // A live PTY job is re-attached, never doubled.
    let st = host.act(TERM_ID, json!({"type": "status"})).await;
    if let Some(e) = st.result.get("error").and_then(Value::as_str) {
        return Err(format!("`{TERM_ID}` status: {e}"));
    }
    if let Some(job_id) = live_pty(&st.result) {
        let r = host.act(TERM_ID, json!({"type": "output", "job_id": job_id})).await;
        if let Some(e) = r.result.get("error").and_then(Value::as_str) {
            return Err(format!("`{TERM_ID}` output: {e}"));
        }
        let replay = r.result.get("output").and_then(Value::as_str).unwrap_or_default().to_string();
        return Ok(Attach { job_id, replay });
    }

    let r = host
        .act(
            TERM_ID,
            json!({"type": "start", "command": sh, "pty": {"cols": cols, "rows": rows}}),
        )
        .await;
    if let Some(e) = r.result.get("error").and_then(Value::as_str) {
        let why = r.result.get("reason").and_then(Value::as_str).unwrap_or(e);
        return Err(format!("`{TERM_ID}` did not start {sh}: {why}"));
    }
    match r.result.get("job_id").and_then(Value::as_i64) {
        Some(j) => Ok(Attach { job_id: j, replay: String::new() }),
        None => Err(format!("`{TERM_ID}` start answered no job_id: {}", r.result)),
    }
}

/// The first running PTY job in a `status` reply.
fn live_pty(status: &Value) -> Option<i64> {
    status.get("jobs")?.as_array()?.iter().find_map(|j| {
        let live = j.get("pty").and_then(Value::as_bool) == Some(true)
            && j.get("status").and_then(Value::as_str) == Some("running");
        live.then(|| j.get("job_id").and_then(Value::as_i64)).flatten()
    })
}

/// Delete `term`; its PTY job dies with it. The human's `can_call` entry is left:
/// a grant to an id that does not exist reaches nothing, and the next `t`
/// reuses it.
pub async fn close(host: &Host) -> Result<(), String> {
    let r = host
        .operator("kernel", json!({"type": "delete_agent", "id": TERM_ID}))
        .await;
    if let Some(e) = r.result.get("error").and_then(Value::as_str) {
        return Err(format!("could not close `{TERM_ID}`: {e}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
