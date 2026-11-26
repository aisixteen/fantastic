//! TOKEN ACCOUNTING, chat-side (#766, spec 1.7): one counter for both pass modes.
//!
//! Every model request (a turn attempt, a task pass) clears the provider's counts before
//! it and reads the endpoint's report after its stream ended ([`crate::model_format::pass`]).
//! [`Counts::of`] takes four disjoint counts from that report: `prompt` (prompt tokens
//! the cache neither served nor wrote, saturating), `completion`, `cache_read`,
//! `cache_write`. A request with no report adds nothing: no estimate ever enters here.
//!
//! The assistant row keeps `attributes.used = {pass, running}`, each a [`Used`]:
//! - `turn.accepted`: the attempt whose answer the chat took (a model-written `stop`
//!   included); nothing in a turn the harness stopped;
//! - `turn.rejected`: every malformed attempt, the attempt a type restart discarded, all
//!   attempts of a turn the harness stopped, and the attempts of turns that stored no row;
//! - `task`: every task pass since the previous stored assistant row;
//! - `total`: the sum of the three, per count: the true spend.
//!
//! Counts with no row yet wait in RAM ([`Pending`], `BackendState.used_pending`) and land
//! on the next stored assistant row's `pass` ([`land`]); `running` is the chat's sum since
//! its first pass (`BackendState.used_running`), seeded after a restart from the newest
//! assistant row ([`seed`]). A process that dies loses what was pending.
//!
//! `attributes.window` is attempt 1's `{prompt, cache_read, cache_write}` of the sequence
//! the stored row came from: the request the view was built for ([`Window`]). The
//! calibration and the `done` status read it.

use std::sync::Arc;

use fantastic_kernel::{AgentId, Kernel};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::provider::ProviderReport;
use crate::state::BackendState;

/// Four disjoint token counts of one request, or a sum of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Counts {
    /// Prompt tokens neither served from nor written to the endpoint's cache.
    pub(crate) prompt: i64,
    /// Output tokens.
    pub(crate) completion: i64,
    /// Prompt tokens served from the endpoint's cache.
    pub(crate) cache_read: i64,
    /// Prompt tokens written to the endpoint's cache.
    pub(crate) cache_write: i64,
}

impl Counts {
    /// The counts of the request whose report this is, `None` when it holds none. A
    /// report holds counts when it names a prompt size or a cache count: those are the
    /// figures a request clears first (`clear_prompt_tokens`), so a report without them
    /// is not this request's, and its `completion_tokens` (which nothing clears) is
    /// never read for it.
    #[must_use]
    pub(crate) fn of(report: &ProviderReport) -> Option<Self> {
        let reported = report.prompt_tokens.is_some_and(|n| n > 0)
            || report.cache_read.is_some()
            || report.cache_write.is_some();
        if !reported {
            return None;
        }
        let cache_read = report.cache_read.unwrap_or(0).max(0);
        let cache_write = report.cache_write.unwrap_or(0).max(0);
        let prompt = report
            .prompt_tokens
            .unwrap_or(0)
            .saturating_sub(cache_read.saturating_add(cache_write))
            .max(0);
        Some(Self {
            prompt,
            completion: report.completion_tokens.unwrap_or(0).max(0),
            cache_read,
            cache_write,
        })
    }

    /// The sum, per count.
    #[must_use]
    pub(crate) fn plus(self, other: Self) -> Self {
        Self {
            prompt: self.prompt.saturating_add(other.prompt),
            completion: self.completion.saturating_add(other.completion),
            cache_read: self.cache_read.saturating_add(other.cache_read),
            cache_write: self.cache_write.saturating_add(other.cache_write),
        }
    }

    /// The difference, per count, never below 0.
    #[must_use]
    pub(crate) fn minus(self, other: Self) -> Self {
        Self {
            prompt: self.prompt.saturating_sub(other.prompt).max(0),
            completion: self.completion.saturating_sub(other.completion).max(0),
            cache_read: self.cache_read.saturating_sub(other.cache_read).max(0),
            cache_write: self.cache_write.saturating_sub(other.cache_write).max(0),
        }
    }

    /// Every count is 0.
    #[must_use]
    pub(crate) fn is_zero(self) -> bool {
        self == Self::default()
    }
}

/// The turn part of a [`Used`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Turn {
    /// The attempt whose answer the chat took.
    pub(crate) accepted: Counts,
    /// Every attempt thrown away.
    pub(crate) rejected: Counts,
}

/// One `pass` or `running` group: `{turn: {accepted, rejected}, task, total}`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Used {
    /// The agent's turn attempts.
    pub(crate) turn: Turn,
    /// The harness's helper passes.
    pub(crate) task: Counts,
    /// `turn.accepted + turn.rejected + task`, per count.
    pub(crate) total: Counts,
}

impl Used {
    /// The group of these parts, its `total` their sum.
    #[must_use]
    pub(crate) fn new(accepted: Counts, rejected: Counts, task: Counts) -> Self {
        Self {
            turn: Turn { accepted, rejected },
            task,
            total: accepted.plus(rejected).plus(task),
        }
    }

    /// The sum of two groups, part by part.
    #[must_use]
    pub(crate) fn plus(self, other: Self) -> Self {
        Self::new(
            self.turn.accepted.plus(other.turn.accepted),
            self.turn.rejected.plus(other.turn.rejected),
            self.task.plus(other.task),
        )
    }

    /// The group as a row keeps it.
    #[must_use]
    pub(crate) fn to_value(self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }

    /// A stored group read back; its `total` is recomputed from its parts.
    #[must_use]
    pub(crate) fn from_value(v: &Value) -> Option<Self> {
        let u: Self = serde_json::from_value(v.clone()).ok()?;
        Some(Self::new(u.turn.accepted, u.turn.rejected, u.task))
    }
}

/// Attempt 1's prompt side of the sequence a stored row came from: the request the view
/// was built for. A cache count the endpoint has no such figure for is absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    /// Prompt tokens neither served from nor written to the cache.
    pub(crate) prompt: i64,
    /// Prompt tokens served from the cache, when the endpoint reports it.
    pub(crate) cache_read: Option<i64>,
    /// Prompt tokens written to the cache, when the endpoint reports it.
    pub(crate) cache_write: Option<i64>,
}

impl Window {
    /// The window of a request's report, `None` when it holds no counts ([`Counts::of`]).
    #[must_use]
    pub(crate) fn of(report: &ProviderReport) -> Option<Self> {
        let c = Counts::of(report)?;
        Some(Self {
            prompt: c.prompt,
            cache_read: report.cache_read.map(|_| c.cache_read),
            cache_write: report.cache_write.map(|_| c.cache_write),
        })
    }

    /// The prompt the request held, all three parts: what the calibration divides and
    /// the `done` status's `prompt_tokens`.
    #[must_use]
    pub(crate) fn sum(self) -> i64 {
        self.prompt
            .saturating_add(self.cache_read.unwrap_or(0))
            .saturating_add(self.cache_write.unwrap_or(0))
    }

    /// The window as a row keeps it.
    #[must_use]
    pub(crate) fn to_value(self) -> Value {
        let mut m = Map::new();
        m.insert("prompt".into(), json!(self.prompt));
        if let Some(n) = self.cache_read {
            m.insert("cache_read".into(), json!(n));
        }
        if let Some(n) = self.cache_write {
            m.insert("cache_write".into(), json!(n));
        }
        Value::Object(m)
    }
}

/// Counts with no row yet: they land on the next stored assistant row's `pass`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pending {
    /// Turn attempts thrown away, and the attempts of turns that stored no row.
    pub(crate) rejected: Counts,
    /// Task passes.
    pub(crate) task: Counts,
}

/// A thrown-away turn attempt's report: its counts wait for the next row's
/// `turn.rejected`.
pub(crate) fn reject(state: &BackendState, report: &ProviderReport) {
    if let Some(c) = Counts::of(report) {
        let mut p = state.used_pending.lock().expect("used_pending poisoned");
        p.rejected = p.rejected.plus(c);
    }
}

/// A task pass's report: its counts wait for the next row's `task`.
pub(crate) fn task(state: &BackendState, report: &ProviderReport) {
    if let Some(c) = Counts::of(report) {
        let mut p = state.used_pending.lock().expect("used_pending poisoned");
        p.task = p.task.plus(c);
    }
}

/// THE ACCEPTED ATTEMPT'S COUNTS, held until its row is stored ([`land`] takes them).
/// Dropped untaken (a cancel or a store failure before the row), they go
/// to `turn.rejected` of the next row, as every attempt of a turn that stored no row.
pub(crate) struct Accepted {
    state: Arc<BackendState>,
    counts: Option<Counts>,
}

impl Accepted {
    /// The accepted attempt of `state`'s turn, with its report.
    #[must_use]
    pub(crate) fn hold(state: &Arc<BackendState>, report: &ProviderReport) -> Self {
        Self {
            state: Arc::clone(state),
            counts: Counts::of(report),
        }
    }

    /// The counts, taken: the row is being stored.
    fn take(mut self) -> Option<Counts> {
        self.counts.take()
    }
}

impl Drop for Accepted {
    fn drop(&mut self) {
        if let Some(c) = self.counts.take() {
            if let Ok(mut p) = self.state.used_pending.lock() {
                p.rejected = p.rejected.plus(c);
            }
        }
    }
}

/// The counts one stored assistant row carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Landed {
    /// This row's pass: the accepted attempt and everything pending.
    pub(crate) pass: Used,
    /// Since the chat's first pass, this row's pass included.
    pub(crate) running: Used,
}

impl Landed {
    /// `attributes.used`.
    #[must_use]
    pub(crate) fn to_value(self) -> Value {
        json!({"pass": self.pass.to_value(), "running": self.running.to_value()})
    }
}

/// `running` seeded once per state, before the chat's first row of this process: the
/// newest assistant row's `used.running`, else nothing yet.
pub(crate) async fn seed(self_id: &AgentId, state: &BackendState, kernel: &Arc<Kernel>) {
    if state
        .used_running
        .lock()
        .expect("used_running poisoned")
        .is_some()
    {
        return;
    }
    let seeded = crate::row_attrs::seed_used(self_id, kernel)
        .await
        .unwrap_or_default();
    let mut r = state.used_running.lock().expect("used_running poisoned");
    if r.is_none() {
        *r = Some(seeded);
    }
}

/// THE COUNTS OF THE ROW BEING STORED: the accepted attempt's (none for a turn the
/// harness stopped) and everything pending. They are taken only once the row is stored
/// ([`Landing::stored`]): a row the store refused takes nothing, and the accepted
/// attempt's counts then wait in `turn.rejected` like any turn that stored no row.
#[must_use]
pub(crate) fn land(state: &Arc<BackendState>, accepted: Option<Accepted>) -> Landing {
    let accepted = accepted.and_then(Accepted::take).unwrap_or_default();
    let pending = *state.used_pending.lock().expect("used_pending poisoned");
    let pass = Used::new(accepted, pending.rejected, pending.task);
    let running = state
        .used_running
        .lock()
        .expect("used_running poisoned")
        .unwrap_or_default()
        .plus(pass);
    Landing {
        state: Arc::clone(state),
        accepted,
        pending,
        landed: Landed { pass, running },
        done: false,
    }
}

/// The counts of a row on its way to the store ([`land`]).
pub(crate) struct Landing {
    state: Arc<BackendState>,
    accepted: Counts,
    pending: Pending,
    /// What the row carries.
    pub(crate) landed: Landed,
    done: bool,
}

impl Landing {
    /// The row is stored: what it carries leaves `used_pending` (a task pass that ended
    /// meanwhile stays) and `running` is the row's.
    pub(crate) fn stored(mut self) {
        self.done = true;
        let mut p = self
            .state
            .used_pending
            .lock()
            .expect("used_pending poisoned");
        p.rejected = p.rejected.minus(self.pending.rejected);
        p.task = p.task.minus(self.pending.task);
        drop(p);
        *self
            .state
            .used_running
            .lock()
            .expect("used_running poisoned") = Some(self.landed.running);
    }
}

impl Drop for Landing {
    fn drop(&mut self) {
        if self.done || self.accepted.is_zero() {
            return;
        }
        if let Ok(mut p) = self.state.used_pending.lock() {
            p.rejected = p.rejected.plus(self.accepted);
        }
    }
}
