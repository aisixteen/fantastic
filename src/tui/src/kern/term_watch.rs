//! THE LAST LAUNCHED COMMAND of each terminal (owner 2026-10-12).
//!
//! A canvas terminal is a login shell (`$SHELL -l`, an exec_bridge PTY job). Once
//! a second this watches every such shell for the command it launched: the
//! shell's child that LEADS THE TERMINAL'S FOREGROUND process group — what you
//! ran, not a prompt's helper (those stay in the shell's own group). A new one
//! is saved on the exec agent's record (`last_cmd`, it survives a restart) and
//! emitted on it (`{type: "last_cmd", cmd}`): the terminal cell shows it, and
//! after a restart offers to run it again. A Windows console has no foreground
//! process group: there the watch sees none (`sys::process_table`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fantastic_kernel::AgentId;
use serde_json::{json, Value};

use crate::kern::host::Host;

const TICK: Duration = Duration::from_secs(1);

/// One process row: `pid ppid pgid tpgid args…`.
#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub tpgid: i64,
    pub args: String,
}

/// `ps -A -o pid=,ppid=,pgid=,tpgid=,args=` parsed; a malformed row is skipped.
pub fn parse_ps(out: &str) -> Vec<Proc> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let pgid = it.next()?.parse().ok()?;
            let tpgid = it.next()?.parse().ok()?;
            let args = it.collect::<Vec<_>>().join(" ");
            (!args.is_empty()).then_some(Proc {
                pid,
                ppid,
                pgid,
                tpgid,
                args,
            })
        })
        .collect()
}

/// The command shell `shell` runs in the foreground: its child that leads its
/// own process group, that group being the terminal's foreground one. None: the
/// shell is at its prompt.
pub fn foreground_child(procs: &[Proc], shell: u32) -> Option<&Proc> {
    procs
        .iter()
        .filter(|p| p.ppid == shell && p.pid == p.pgid && p.tpgid == i64::from(p.pgid))
        .max_by_key(|p| p.pid)
}

/// Every live login-shell PTY job: (its exec agent, the shell's pid).
fn shells() -> Vec<(AgentId, u32)> {
    fantastic_exec_bridge::jobs::jobs()
        .values()
        .filter(|j| j.pty.is_some() && fantastic_exec_bridge::jobs::alive(j) && j.args == ["-l"])
        .map(|j| (j.owner.clone(), j.pid))
        .collect()
}

/// Watch for the life of the process.
pub fn spawn(h: Arc<Host>) {
    tokio::spawn(async move {
        // per exec agent: the pid of the last command seen, so one long run is one launch
        let mut seen: HashMap<AgentId, u32> = HashMap::new();
        loop {
            tokio::time::sleep(TICK).await;
            let shells = shells();
            if shells.is_empty() {
                continue;
            }
            let Some(out) = tokio::task::spawn_blocking(crate::sys::process_table)
                .await
                .ok()
                .flatten()
            else {
                continue;
            };
            let procs = parse_ps(&out);
            for (agent, shell) in shells {
                let Some(fg) = foreground_child(&procs, shell) else {
                    continue;
                };
                if seen.get(&agent) == Some(&fg.pid) {
                    continue;
                }
                seen.insert(agent.clone(), fg.pid);
                let cmd = fg.args.clone();
                let r = h
                    .operator(
                        "kernel",
                        json!({"type": "update_agent", "id": agent.as_str(), "last_cmd": cmd}),
                    )
                    .await
                    .result;
                if let Some(e) = r.get("error").and_then(Value::as_str) {
                    tracing::warn!("last_cmd on {agent}: {e}");
                    continue;
                }
                h.kernel
                    .emit(&agent, json!({"type": "last_cmd", "cmd": cmd}))
                    .await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "    1     0     1     -1 /sbin/launchd\n  500   400   500    700 -zsh\n  610   500   500    700 starship prompt --status=0\n  700   500   700    700 npm run dev\n  701   700   700    700 node server.js\n  800   500   800    700 \n garbage row\n";

    #[test]
    fn ps_rows_parse_and_a_bad_row_is_skipped() {
        let p = parse_ps(PS);
        assert_eq!(p.len(), 5);
        assert_eq!(
            p[3],
            Proc {
                pid: 700,
                ppid: 500,
                pgid: 700,
                tpgid: 700,
                args: "npm run dev".into()
            }
        );
        assert_eq!(p[0].tpgid, -1);
    }

    #[test]
    fn the_foreground_child_is_the_launched_command_not_a_prompt_helper() {
        let p = parse_ps(PS);
        assert_eq!(
            foreground_child(&p, 500).map(|x| x.args.as_str()),
            Some("npm run dev")
        );
        // the command's own child is not the shell's
        assert_eq!(foreground_child(&p, 700), None);
    }

    #[test]
    fn at_the_prompt_there_is_none() {
        // only the prompt helper (in the shell's own group) runs
        let p = parse_ps(
            "  500   400   500    500 -zsh\n  610   500   500    500 git status --porcelain\n",
        );
        assert_eq!(foreground_child(&p, 500), None);
        // a background job (its own group, not the foreground one) is not it either
        let p = parse_ps("  500   400   500    500 -zsh\n  620   500   620    500 sleep 100\n");
        assert_eq!(foreground_child(&p, 500), None);
    }
}
