//! The four states an agent can be in — and never a fifth.
//!
//! herdr marks a pane working/blocked/done/idle/**unknown**, because it infers
//! state from the foreground process and the screen. Here the state is a kernel
//! FACT: a task table row, an unanswered message, a settled root. There is
//! nothing to guess, so `Unknown` does not exist.

// `label` names the four states for a legend. Declared with them so the set stays
// in one place.
#![allow(dead_code)]

use crate::kern::model::{AgentId, Task};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Has at least one task of its own that is real work.
    Working,
    /// Someone is waiting on an answer from this agent.
    Blocked,
    /// Finished something you have not looked at yet.
    Done,
    /// Nothing running, nothing waiting, nothing new.
    Idle,
}

impl Presence {
    pub fn label(self) -> &'static str {
        match self {
            Presence::Working => "working",
            Presence::Blocked => "blocked",
            Presence::Done => "done",
            Presence::Idle => "idle",
        }
    }
}

/// Task labels the kernel's own plumbing holds open for the life of an agent.
/// A watcher drain loop or a ledger flush is not "this agent is working" — and
/// counting it would leave half the tree permanently green.
pub const INFRA_VERBS: &[&str] = &[
    "ws_drain",
    "ws_call",
    "read_loop",
    "tick_loop",
    "pty_reader",
    "ledger_flush",
    "flush_loop",
    "message_pump",
];

/// Is this task real work, or the kernel holding its own plumbing open?
pub fn is_work(t: &Task) -> bool {
    !INFRA_VERBS.contains(&t.verb.as_str())
}

/// `blocked` outranks `working`: an agent that is both computing and waiting on
/// you is, from your side of the screen, waiting on you.
pub fn of(agent: &AgentId, tasks: &[Task], waiting_on_me: usize, done: bool) -> Presence {
    if waiting_on_me > 0 {
        return Presence::Blocked;
    }
    if tasks.iter().any(|t| &t.agent == agent && is_work(t)) {
        return Presence::Working;
    }
    if done {
        Presence::Done
    } else {
        Presence::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(agent: &str, verb: &str) -> Task {
        Task {
            id: 1,
            agent: agent.into(),
            parent: None,
            verb: verb.into(),
            started_at: 0.0,
        }
    }

    #[test]
    fn never_unknown() {
        // Every combination lands on one of exactly four.
        for waiting in [0usize, 1] {
            for done in [false, true] {
                for tasks in [vec![], vec![task("ai", "send")]] {
                    let p = of(&"ai".to_string(), &tasks, waiting, done);
                    assert!(matches!(
                        p,
                        Presence::Working | Presence::Blocked | Presence::Done | Presence::Idle
                    ));
                }
            }
        }
    }

    #[test]
    fn infra_verbs_do_not_mean_working() {
        let t = vec![task("ai", "ws_drain"), task("ai", "ledger_flush")];
        assert_eq!(of(&"ai".to_string(), &t, 0, false), Presence::Idle);
        let t = vec![task("ai", "ws_drain"), task("ai", "send")];
        assert_eq!(of(&"ai".to_string(), &t, 0, false), Presence::Working);
    }

    #[test]
    fn another_agents_task_is_not_mine() {
        let t = vec![task("other", "send")];
        assert_eq!(of(&"ai".to_string(), &t, 0, false), Presence::Idle);
    }

    #[test]
    fn open_inquiry_beats_running() {
        let t = vec![task("ai", "send")];
        assert_eq!(of(&"ai".to_string(), &t, 1, false), Presence::Blocked);
    }

    #[test]
    fn closed_unseen_is_done_then_idle_once_seen() {
        assert_eq!(of(&"ai".to_string(), &[], 0, true), Presence::Done);
        assert_eq!(of(&"ai".to_string(), &[], 0, false), Presence::Idle);
    }
}
