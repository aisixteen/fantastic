//! A PTY `start` with no `command` runs the CHILD's own `$SHELL` — twins of
//! python `exec_bridge/tests/test_shell_autodiscovery.py`.
//!
//! The shell is AUTODISCOVERED from the environment the record passes down
//! (`inherit_env`/`env`) — never from the kernel's env unless inherited, never
//! from a fallback chain. The allowlist still decides: the record's `shell`
//! field (`true` → `[$SHELL]`, `"login"` → `[$SHELL, "-l"]`) permits that exact
//! line and nothing wider; every other argv, and every pipe job, still needs
//! `allowed`.
//!
//! The "shell" here is a tiny executable script, so the tests read the same on
//! every OS: it prints its argv and echoes one typed line back.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use fantastic_kernel::{Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

use crate::{jobs, ExecBridgeBundle, HANDLER_MODULE};

fn pty() -> Value {
    json!({"cols": 80, "rows": 24})
}

/// An executable that stands in for a login shell: it announces its argv, then
/// echoes one typed line — enough to prove WHICH line was spawned and that it
/// sits on the terminal.
fn fake_shell(tmp: &TempDir) -> String {
    let path = tmp.path().join("fakesh");
    std::fs::write(
        &path,
        "#!/bin/sh\necho \"SHELL_UP $*\"\nread line\necho \"GOT $line\"\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().to_string()
}

/// A kernel with only the root; agents are created per test with exactly the
/// record fields the case is about.
fn kernel(tmp: &TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ExecBridgeBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

/// `create_agent` an open exec leg with `extra` on its record; returns the reply.
async fn mk(kernel: &Arc<Kernel>, tmp: &TempDir, id: &str, extra: Value) -> Value {
    let mut create = json!({
        "type": "create_agent", "parent_id": "kernel", "id": id,
        "handler_module": HANDLER_MODULE, "ingress_rule": "allow_all_ingress",
        "cwd": tmp.path().to_string_lossy(),
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        create[k] = v;
    }
    call(kernel, "kernel_state", create).await
}

async fn call(kernel: &Arc<Kernel>, id: &str, p: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(id), p)])
        .await
        .remove(0)
        .result
}

/// SIGKILL every job an agent owns, so a shell a test started never outlives it.
fn teardown(id: &str) {
    let mut g = jobs::jobs();
    for j in g.values_mut().filter(|j| j.owner.as_str() == id) {
        jobs::teardown(j);
    }
}

/// Drain the agent's inbox until `pred(output_text)` holds; returns the output
/// and every event payload seen on the way.
async fn events_until(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    pred: impl Fn(&str) -> bool,
) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !pred(&text) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out; got {text:?}"
        );
        if let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            if item.payload["type"] == "output" {
                text.push_str(item.payload["data"].as_str().unwrap_or(""));
            }
            events.push(item.payload);
        }
    }
    (text, events)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pty_start_without_command_runs_the_childs_shell() {
    let tmp = TempDir::new().unwrap();
    let sh = fake_shell(&tmp);
    let k = kernel(&tmp);
    mk(
        &k,
        &tmp,
        "sh_run",
        json!({"env": {"SHELL": sh}, "shell": true}),
    )
    .await;
    let mut rx = k.ensure_inbox(&AgentId::from("sh_run"));
    let r = call(&k, "sh_run", json!({"type": "start", "pty": pty()})).await;
    assert_eq!(
        (&r["status"], &r["pty"]),
        (&json!("running"), &json!(true)),
        "{r}"
    );
    let jid = r["job_id"].as_i64().unwrap();
    let (_, events) = events_until(&mut rx, |t| t.contains("SHELL_UP")).await;
    // The announcement names the RESOLVED shell, so a client that let the kernel
    // pick it can show it and re-issue it.
    let started = events
        .iter()
        .find(|e| e["type"] == "job_started")
        .expect("job_started");
    assert_eq!(
        started,
        &json!({"type": "job_started", "job_id": jid, "command": sh, "args": []})
    );
    let snap = &call(&k, "sh_run", json!({"type": "status", "job_id": jid})).await["jobs"][0];
    assert_eq!(
        (&snap["command"], &snap["args"], &snap["pty"]),
        (&json!(sh), &json!([]), &json!(true)),
        "{snap}"
    );
    // It is a real terminal job: a typed line reaches the shell.
    call(
        &k,
        "sh_run",
        json!({"type": "write", "job_id": jid, "data": "hi\n"}),
    )
    .await;
    events_until(&mut rx, |t| t.contains("GOT hi")).await;
    teardown("sh_run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_shell_comes_from_the_childs_env_not_the_kernels() {
    // Whatever SHELL this process has, the record passes nothing down — so the
    // child has none, and the start is refused naming where it would come from.
    // (The process env is not mutated: other tests in this binary spawn shells.)
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp);
    mk(&k, &tmp, "sh_env", json!({"shell": true})).await;
    let r = call(&k, "sh_env", json!({"type": "start", "pty": pty()})).await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("missing_arg"), &json!("SHELL")),
        "{r}"
    );
    assert!(
        r["hint"]
            .as_str()
            .is_some_and(|h| h.contains("inherit_env")),
        "{r}"
    );
    // Passing it down by NAME is the whole repair — when this process has one.
    let Ok(kernel_shell) = std::env::var("SHELL") else {
        return;
    };
    if kernel_shell.is_empty() {
        return;
    }
    let u = call(
        &k,
        "kernel_state",
        json!({"type": "update_agent", "id": "sh_env", "inherit_env": ["SHELL"]}),
    )
    .await;
    assert!(u.get("error").is_none(), "{u}");
    let r = call(&k, "sh_env", json!({"type": "start", "pty": pty()})).await;
    assert_eq!(r["status"], "running", "{r}");
    let snap = &call(
        &k,
        "sh_env",
        json!({"type": "status", "job_id": r["job_id"]}),
    )
    .await["jobs"][0];
    assert_eq!(snap["command"], json!(kernel_shell), "{snap}");
    teardown("sh_env");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_shell_in_the_child_env_is_refused_too() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp);
    mk(
        &k,
        &tmp,
        "sh_empty",
        json!({"env": {"SHELL": ""}, "shell": true}),
    )
    .await;
    let r = call(&k, "sh_empty", json!({"type": "start", "pty": pty()})).await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("missing_arg"), &json!("SHELL")),
        "{r}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pipe_job_always_names_its_command() {
    // Autodiscovery is PTY-only: a shell on a pipe has no terminal to talk to.
    let tmp = TempDir::new().unwrap();
    let sh = fake_shell(&tmp);
    let k = kernel(&tmp);
    mk(
        &k,
        &tmp,
        "sh_pipe",
        json!({"env": {"SHELL": sh}, "shell": true}),
    )
    .await;
    let r = call(&k, "sh_pipe", json!({"type": "start"})).await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("missing_arg"), &json!("command")),
        "{r}"
    );
    // …and an explicit empty command on a PTY is a typo, not "use the shell".
    let r = call(
        &k,
        "sh_pipe",
        json!({"type": "start", "command": "", "pty": pty()}),
    )
    .await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("missing_arg"), &json!("command")),
        "{r}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_true_permits_only_the_bare_shell_line() {
    let tmp = TempDir::new().unwrap();
    let sh = fake_shell(&tmp);
    let k = kernel(&tmp);
    mk(
        &k,
        &tmp,
        "sh_bare",
        json!({"env": {"SHELL": sh}, "shell": true}),
    )
    .await;
    // extra args are a different line — `shell` does not cover it
    let r = call(
        &k,
        "sh_bare",
        json!({"type": "start", "args": ["-c", "id"], "pty": pty()}),
    )
    .await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    let err = r["error"].as_str().unwrap_or("");
    assert!(err.contains("permits only the PTY line"), "{r}");
    // The value is spelled as JSON, the same bytes in every runtime.
    assert!(
        err.contains("(`shell=true` permits only the PTY line "),
        "{r}"
    );
    // the same shell on a PIPE is not the permitted line either
    let r = call(&k, "sh_bare", json!({"type": "start", "command": sh})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    // naming the shell explicitly on a PTY is the same line, so it is permitted
    let r = call(
        &k,
        "sh_bare",
        json!({"type": "start", "command": sh, "pty": pty()}),
    )
    .await;
    assert_eq!(r["status"], "running", "{r}");
    teardown("sh_bare");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_login_permits_exactly_the_login_line() {
    let tmp = TempDir::new().unwrap();
    let sh = fake_shell(&tmp);
    let k = kernel(&tmp);
    mk(
        &k,
        &tmp,
        "sh_login",
        json!({"env": {"SHELL": sh}, "shell": "login"}),
    )
    .await;
    let mut rx = k.ensure_inbox(&AgentId::from("sh_login"));
    // the bare shell is NOT the login line
    let r = call(&k, "sh_login", json!({"type": "start", "pty": pty()})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    let err = r["error"].as_str().unwrap_or("");
    assert!(
        err.contains("-l") && err.contains("`shell=\"login\"`"),
        "{r}"
    );
    let r = call(
        &k,
        "sh_login",
        json!({"type": "start", "args": ["-l"], "pty": pty()}),
    )
    .await;
    assert_eq!(r["status"], "running", "{r}");
    events_until(&mut rx, |t| t.contains("SHELL_UP -l")).await;
    teardown("sh_login");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_shell_the_resolved_line_still_needs_allowed() {
    // No `shell` on the record: the autodiscovered line is judged by `allowed`
    // like any other — autodiscovery never widens the gate.
    let tmp = TempDir::new().unwrap();
    let sh = fake_shell(&tmp);
    let k = kernel(&tmp);
    mk(&k, &tmp, "sh_allow", json!({"env": {"SHELL": sh}})).await;
    let r = call(&k, "sh_allow", json!({"type": "start", "pty": pty()})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    // No `shell` ⇒ no shell suffix on the refusal.
    assert!(
        !r["error"]
            .as_str()
            .unwrap_or("")
            .contains("permits only the PTY line"),
        "{r}"
    );
    // `allowed` is a regex: escape the path so its `.`/`/` match literally.
    let u = call(
        &k,
        "kernel_state",
        json!({"type": "update_agent", "id": "sh_allow", "allowed": [fancy_regex::escape(&sh)]}),
    )
    .await;
    assert!(u.get("error").is_none(), "{u}");
    let r = call(&k, "sh_allow", json!({"type": "start", "pty": pty()})).await;
    assert_eq!(r["status"], "running", "{r}");
    teardown("sh_allow");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_does_not_disable_the_allowlist() {
    let tmp = TempDir::new().unwrap();
    let sh = fake_shell(&tmp);
    let k = kernel(&tmp);
    mk(
        &k,
        &tmp,
        "sh_list",
        json!({"env": {"SHELL": sh}, "shell": true, "allowed": ["/bin/echo .*"]}),
    )
    .await;
    let r = call(
        &k,
        "sh_list",
        json!({"type": "start", "command": "/bin/echo", "args": ["x"]}),
    )
    .await;
    assert_eq!(r["status"], "running", "{r}");
    teardown("sh_list");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_is_refused_at_write_unless_true_false_or_login() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp);
    for (i, bad) in [json!("bash"), json!(1), json!("yes"), json!(["login"])]
        .into_iter()
        .enumerate()
    {
        let r = mk(&k, &tmp, &format!("bad_{i}"), json!({"shell": bad})).await;
        assert_eq!(r["reason"], "invalid_meta", "{bad}: {r}");
        assert!(
            r["error"]
                .as_str()
                .is_some_and(|e| e.contains("legal values")),
            "{r}"
        );
    }
    for (i, ok) in [json!(true), json!(false), json!("login")]
        .into_iter()
        .enumerate()
    {
        let r = mk(&k, &tmp, &format!("ok_{i}"), json!({"shell": ok})).await;
        assert!(r["id"].is_string(), "{ok}: {r}");
    }
    // the exact python text, `repr` of the refused value included
    let r = mk(&k, &tmp, "bad_text", json!({"shell": "bash"})).await;
    assert_eq!(
        r["error"],
        json!(
            "shell='bash' is not a shell permission — legal values: true (the child's \
             $SHELL, bare), \"login\" ($SHELL -l), false (none)"
        ),
        "{r}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_advertises_the_shell_permission() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp);
    mk(&k, &tmp, "plain", json!({})).await;
    mk(&k, &tmp, "login", json!({"shell": "login"})).await;
    assert_eq!(
        call(&k, "plain", json!({"type": "reflect"})).await["shell"],
        json!(false)
    );
    assert_eq!(
        call(&k, "login", json!({"type": "reflect"})).await["shell"],
        json!("login")
    );
}
