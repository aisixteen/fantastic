//! THE MESSAGE ALPHABET. Everything that can happen to this shell is one of
//! these, and everything the shell wants done is a `Cmd`.
//!
//! The rule this file exists to enforce: **a key handler and a panel never
//! await.** They push a `Cmd` and return. One task owns the wire, turns a `Cmd`
//! into a request and posts the reply back as a `Got`. That is why a slow kernel
//! can never freeze a keystroke, and why there is exactly one place to look when
//! a request goes missing.

// THE WATCH LEG: `Cmd::Watch(agent)` registers a synthesized reader inbox per
// watched agent (a watch frame carries no source id) and `Kernel::watch`es the
// agent into it; each mirrored payload arrives as `LinkEv::AgentEvent`. Its
// first consumer is the in-app terminal.
//
// `Cmd::ManifestPut` is implemented end to end in the runner and merely has no
// keybinding: adding a ui agent is a manifest edit plus a roster boot.
#![allow(dead_code)]

use serde_json::Value;

use crate::kern::model::{AgentId, KernelErr, Record, Row, RowId, StateEvent, TreeNode};

/// Everything the main loop selects over.
#[derive(Debug)]
pub enum Ev {
    /// The terminal. Crossterm's types STOP HERE — no pane sees a raw event
    /// without going through the keymap first.
    Term(crossterm::event::Event),
    /// 60 s. A full re-read, as the safety net under the live feed. If a frame
    /// was ever missed, this is what heals it, so no other code has to be
    /// clever about gaps.
    Recon,
    Link(LinkEv),
    Got(Got),
    /// A loud message from a task that holds no `App`: a boot note, a refusal a
    /// runner could not attribute. It lands in the note list like any other
    /// failure.
    Note(String),
}

/// What the kernel pushes at this shell while it runs. Both of these are FACTS
/// rather than timers: the row tail fires because rows were appended, the state
/// event because something was dispatched.
#[derive(Debug)]
pub enum LinkEv {
    State(StateEvent),
    /// The kernel appended rows to this agent. Pushed by its own row tail, so the
    /// shell re-reads exactly when something happened and never on a guess.
    RowsMoved {
        agent: AgentId,
    },
    /// A payload an agent received, from its inbox watch.
    AgentEvent {
        agent: AgentId,
        payload: Value,
    },
}

/// A reply to a `Cmd`. Note what is ABSENT: nothing here is a "pending" or an
/// "answer". A question to the human is just a row, and being asked is a QUERY
/// over rows, so there is no ticket state to keep and no future to park.
#[derive(Debug)]
pub enum Got {
    RootReflect {
        id: AgentId,
        runtime: String,
        /// `None` means NO STORE IS WIRED, which the statusline must say out
        /// loud: rows will not survive a restart.
        provider: Option<String>,
        guid: Option<String>,
    },
    Tree(TreeNode),
    /// The in-app terminal's pane has a PTY job to draw: a fresh one, or a live
    /// one re-attached with what it printed so far (`kern/terminal`).
    TermAttached {
        job_id: i64,
        replay: String,
    },
    Agents {
        records: Vec<Record>,
    },
    Ps(Vec<crate::kern::model::Task>),
    /// `full` tells the store to rebuild its order rather than append. There are
    /// no pages in-process, so there is no cursor here to get wrong.
    Rows {
        agent: AgentId,
        rows: Vec<Row>,
        full: bool,
        /// An AI agent's `rows` said whether it holds its history; `None` for an
        /// agent that keeps no rows of its own.
        loaded: Option<bool>,
    },
    Reflect {
        agent: AgentId,
        reply: Value,
    },
    Sent {
        target: AgentId,
        reply: Value,
    },
    Cancelled {
        killed: Vec<RowId>,
    },
    RosterBooted {
        created: Vec<String>,
        reapplied: Vec<String>,
    },
    /// The backends this build links.
    Backends(Vec<crate::kern::setup::Backend>),
    /// Where provider keys are kept: the mode (`None` until chosen) and where
    /// that is, in words — or why it cannot be read.
    KeyMode(Result<(Option<String>, String), String>),
    /// The user's saved templates, or why they cannot be read.
    Templates(Result<crate::kern::templates::Shelf, String>),
    /// What the set-up page's last write or key store came to.
    Setup(Result<String, String>),
    /// A REFUSAL IS DATA. The kernel's `{"error":…,"reason":…}` is not an
    /// exception to swallow — it is the answer, and it renders where the request
    /// was made so the human sees which grant is missing.
    Refused {
        what: What,
        err: KernelErr,
    },
}

/// Which request a refusal belongs to, so the pane that asked is the pane that
/// shows the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    RootReflect,
    Tree,
    Agents,
    Ps,
    Rows,
    Reflect,
    Send,
    Cancel,
    RosterBoot,
}

/// Outbound intents.
#[derive(Debug, Clone)]
pub enum Cmd {
    RootReflect,
    Tree,
    Agents,
    Ps,
    /// `full: true` re-reads the conversation from row zero; otherwise only the
    /// rows AFTER `after` (the newest id this shell already holds for the agent)
    /// are read — an agent's history only grows, and re-reading all of it on
    /// every tick made each read cost the whole log (8 MB for a busy root).
    Rows {
        agent: AgentId,
        full: bool,
        after: Option<crate::kern::model::RowId>,
    },
    Reflect {
        agent: AgentId,
    },
    /// THE ONE ACT PATH. Everything the human does to the kernel goes out as
    /// `user.send{target,payload}`, so the ledger row and the vouched header
    /// both name a person rather than a transport leg.
    ///
    /// An ANSWER to a question is not a distinct command: it is this, with `re`
    /// set in the payload to the id of the row that asked.
    Send {
        target: AgentId,
        payload: Value,
    },
    Cancel {
        task_id: RowId,
    },
    /// Add or replace one agent entry in `.fantastic/kernel.yaml`, then boot the
    /// roster so the kernel picks it up. Wiring an agent is an EDIT TO THE
    /// MANIFEST, not a hidden runtime call — the file stays the truth.
    ManifestPut {
        id: String,
        entry: serde_yaml::Mapping,
    },
    RosterBoot,
    Watch(AgentId),
    Unwatch(AgentId),
    /// A stream consumer's own housekeeping — a flow-control `ack`, a
    /// `resize` — sent as OPERATOR. It is what a reader does to keep reading,
    /// like drawing a reflect, not something the human did, so it earns no
    /// act row per chunk.
    Stream {
        agent: AgentId,
        payload: Value,
    },
    /// Create (or boot) the in-app terminal at this size. See `kern/terminal`.
    OpenTerminal {
        cols: u16,
        rows: u16,
    },
    CloseTerminal,
    /// The backends this build links, for the set-up page.
    Backends,
    /// Write an AI agent's record from the set-up page (`kern/setup`).
    WriteAi(crate::kern::setup::Record),
    /// Where provider keys are kept (`kern/keys`).
    KeyMode,
    /// Choose (or change) where provider keys are kept: "plain" | "secure".
    SetKeyMode {
        mode: String,
    },
    /// The user's saved templates (`kern/templates`).
    Templates,
    /// Save the set-up page's backend, model and endpoint as template `name`.
    SaveTemplate {
        name: String,
        record: Value,
    },
    /// Make template `name` the default for new AI agents; `None` clears it.
    PreferTemplate {
        name: Option<String>,
    },
    /// Store a provider key in the keychain (`kern/keys`).
    StoreKey {
        module: String,
        /// The endpoint the key is for: a key is bound to its host.
        endpoint: String,
        secret: String,
    },
    /// SHELL-LOCAL, never on the wire: select `agent` and put the cursor on
    /// `row`. The Activity page's Enter. The app handles it before the runner
    /// sees the queue.
    Jump {
        agent: AgentId,
        row: RowId,
    },
    Quit,
}

impl Cmd {
    /// What a refusal to this command should be filed under.
    pub fn what(&self) -> What {
        match self {
            Cmd::RootReflect => What::RootReflect,
            Cmd::Tree => What::Tree,
            Cmd::Agents => What::Agents,
            Cmd::Ps => What::Ps,
            Cmd::Rows { .. } => What::Rows,
            Cmd::Reflect { .. } => What::Reflect,
            Cmd::Send { .. } => What::Send,
            Cmd::Cancel { .. } => What::Cancel,
            Cmd::ManifestPut { .. } | Cmd::RosterBoot => What::RosterBoot,
            Cmd::Watch(_) | Cmd::Unwatch(_) | Cmd::Jump { .. } | Cmd::Quit => What::Ps,
            Cmd::Stream { .. } => What::Send,
            Cmd::OpenTerminal { .. } | Cmd::CloseTerminal => What::RosterBoot,
            Cmd::Backends | Cmd::WriteAi(_) | Cmd::StoreKey { .. } => What::RosterBoot,
            Cmd::Templates | Cmd::SaveTemplate { .. } | Cmd::PreferTemplate { .. } => What::RosterBoot,
            Cmd::KeyMode | Cmd::SetKeyMode { .. } => What::RosterBoot,
        }
    }

    /// TRUE if this command changes the kernel. Acts carry an execution id so a
    /// retry after a dropped link JOINS the original instead of running twice,
    /// and acts get no client-side timeout. Reads get neither.
    pub fn is_act(&self) -> bool {
        matches!(
            self,
            Cmd::Send { .. } | Cmd::Cancel { .. } | Cmd::RosterBoot | Cmd::ManifestPut { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_are_not_acts() {
        for c in [
            Cmd::Tree,
            Cmd::Agents,
            Cmd::Ps,
            Cmd::Rows {
                agent: "ai".into(),
                full: true,
                after: None,
            },
        ] {
            assert!(!c.is_act(), "{c:?} must not carry an execution id");
        }
    }

    #[test]
    fn every_mutation_is_an_act() {
        let send = Cmd::Send {
            target: "ai".into(),
            payload: json!({"type":"send","text":"hi"}),
        };
        assert!(send.is_act());
        assert!(Cmd::Cancel { task_id: 1 }.is_act());
        assert!(Cmd::RosterBoot.is_act());
    }

    #[test]
    fn an_answer_is_an_ordinary_send() {
        // The async law: no `answer` verb exists. Answering a question is a send
        // that points back at the row which asked.
        let answer = Cmd::Send {
            target: "ai".into(),
            payload: json!({"type":"send","text":"8080","re":425}),
        };
        assert_eq!(answer.what(), What::Send);
        assert!(answer.is_act());
    }
}
