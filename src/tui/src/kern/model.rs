//! The kernel's wire types, as they actually arrive.
//!
//! Two rules run through every struct here.
//!
//! **Forward-safe.** A vocabulary the kernel grows tomorrow must not fail a
//! whole page today: `kind`, `status` and `res` stay `String`, and `Row` keeps an
//! `extra` map so an unknown field survives into the Generic panel instead of
//! being dropped on the floor.
//!
//! **Failure is a status, never prose.** `Outcome::failed` compares against
//! `completed` and nothing else. The rule "an error settles completed, so parse
//! the result" is pre-2026-09-11 behaviour and is dead.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type RowId = i64;
pub type AgentId = String;

/// One WHOLE row, read straight off the kernel's row store.
///
/// `kind` is ABSENT on a plain conversation turn; the bookkeeping kinds are
/// `call`, `task`, `body`, `view` and `settle`, and the outline filters exactly
/// those (the same set the kernel's own history loader drops).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Row {
    pub id: RowId,
    #[serde(default)]
    pub agent: AgentId,
    #[serde(default)]
    pub ts: f64,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub parent: Option<RowId>,
    #[serde(default)]
    pub content: Option<String>,
    /// The generation's THINKING, kept on its answer's row (never in the model's
    /// view): shown on demand (`t` in the AI panel).
    #[serde(default)]
    pub thinking: Option<String>,

    // ── call / task rows ─────────────────────────────────────────────────
    #[serde(default)]
    pub verb: Option<String>,
    /// The caller the gate saw. `None` means the operator.
    #[serde(default)]
    pub caller: Option<AgentId>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub internal: Option<bool>,
    /// Stamped in place on a live call row. The durable form is a `settle` row
    /// on the root; both shapes exist in the wild and the store reads both.
    #[serde(default)]
    pub outcome: Option<Outcome>,

    // ── settle rows (root-owned) ─────────────────────────────────────────
    #[serde(default)]
    pub settled: Option<AgentId>,

    // ── fold rows ────────────────────────────────────────────────────────
    #[serde(default)]
    pub refs: Option<Vec<RowId>>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub level: Option<u32>,

    // ── view rows ────────────────────────────────────────────────────────
    #[serde(default)]
    pub v: Option<u32>,
    #[serde(default)]
    pub budget: Option<i64>,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub ids: Option<Vec<ViewEntry>>,

    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The five bookkeeping kinds the conversation view drops.
pub const BOOKKEEPING: &[&str] = &["call", "task", "body", "view", "settle"];

impl Row {
    pub fn kind_is(&self, k: &str) -> bool {
        self.kind.as_deref() == Some(k)
    }
    /// Is this row part of the CONVERSATION — the set the model is shown?
    pub fn is_conversation(&self) -> bool {
        match self.kind.as_deref() {
            None => true,
            Some(k) => !BOOKKEEPING.contains(&k),
        }
    }
    pub fn bytes(&self) -> usize {
        self.content.as_deref().map_or(0, str::len)
    }
}

/// `status` is a STRING, not an enum: the vocabulary is five today and a sixth
/// must not fail a page.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Outcome {
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

pub const COMPLETED: &str = "completed";

impl Outcome {
    /// FAILURE IS A STATUS. Never parse the result.
    pub fn failed(&self) -> bool {
        self.status != COMPLETED
    }
}

/// One entry of a view row's `ids`, already in PROMPT order.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ViewEntry {
    pub id: RowId,
    /// `full` | `stub` | `fold` — a String for the same forward-safety reason.
    pub res: String,
}

/// A `list_agents` element: the agent's record, verbatim.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Record {
    pub id: AgentId,
    #[serde(default)]
    pub parent_id: Option<AgentId>,
    #[serde(default)]
    pub handler_module: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub ingress_rule: Option<String>,
    /// `text_file` → the `file_bridge` it writes through.
    #[serde(default)]
    pub target_id: Option<String>,
    /// A `file_bridge`'s clamp.
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub can_call: Option<Value>,
    #[serde(default)]
    pub callable_by: Option<Value>,
    /// Everything else, rendered verbatim by the Generic panel.
    #[serde(flatten)]
    pub meta: Map<String, Value>,
}

/// One `ps` row. `parent` is deliberately NOT used for attribution: the task
/// table re-parents on forget, while a row's `parent` is immutable.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Task {
    pub id: RowId,
    pub agent: AgentId,
    #[serde(default)]
    pub parent: Option<RowId>,
    #[serde(default)]
    pub verb: String,
    #[serde(default)]
    pub started_at: f64,
}

/// A `reflect detail=full tree=all` node — the ungated walk, and the only shape
/// that carries `handler_module` per node.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct TreeNode {
    pub id: AgentId,
    #[serde(default)]
    pub parent_id: Option<AgentId>,
    #[serde(default)]
    pub handler_module: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// Depth-first walk, parents before children.
    pub fn walk<'a>(&'a self, depth: u8, out: &mut Vec<(&'a TreeNode, u8)>) {
        out.push((self, depth));
        for c in &self.children {
            c.walk(depth.saturating_add(1), out);
        }
    }
}

/// One event off the kernel's own state feed. The kernel publishes these inline
/// as it dispatches: `{type, sender, target, verb, summary}`. `summary` is the
/// full stringified payload, deliberately untruncated.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StateEvent {
    /// `send` | `emit` | `send_binary`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub sender: Option<AgentId>,
    #[serde(default)]
    pub target: Option<AgentId>,
    #[serde(default)]
    pub verb: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

/// A kernel refusal. It arrives as HTTP **200** with this body, and it is DATA:
/// rendered in place, never thrown away, never retried behind your back.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Refusal {
    pub error: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub hint: Option<String>,
    #[serde(default)]
    pub see: Option<String>,
    /// `egress` | `ingress` | `delegation` | `self-grant`
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub arg: Option<String>,
    #[serde(default)]
    pub verb: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Refusal {
    /// The kernel ships a `hint` on an authorization refusal —
    /// discovery-through-denial. Show it verbatim; never paraphrase it.
    pub fn line(&self) -> String {
        match &self.hint {
            Some(h) if !h.is_empty() => format!("{} — {}", self.error, h),
            _ => self.error.clone(),
        }
    }
}

/// What can come back that is not an answer.
///
/// Two cases, because in-process there are only two: the kernel REFUSED (data,
/// rendered where it was asked for) or the kernel could not be brought up at all.
#[derive(Debug, thiserror::Error)]
pub enum KernelErr {
    // BOXED: a `Refusal` carries the kernel's whole hint text, and an unboxed one
    // made every `Result` in the crate expensive to move for the sake of the rare
    // error path.
    #[error("refused: {}", .0.line())]
    Refused(Box<Refusal>),
    #[error("{0}")]
    Boot(String),
    #[error("decode: {0}")]
    Decode(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_deserializes_every_known_kind() {
        let call: Row = serde_json::from_str(
            r#"{"id":4655,"kind":"call","verb":"send","parent":4654,"caller":"user","agent":"ai_b","ts":1.0}"#,
        )
        .unwrap();
        assert_eq!(call.verb.as_deref(), Some("send"));
        assert_eq!(call.caller.as_deref(), Some("user"));
        assert!(!call.is_conversation());

        let turn: Row = serde_json::from_str(
            r#"{"role":"user","id":4660,"agent":"ai_b","ts":1.0,"content":"do it"}"#,
        )
        .unwrap();
        assert!(turn.kind.is_none());
        assert!(turn.is_conversation());

        let fold: Row = serde_json::from_str(
            r#"{"role":"user","id":8132,"parent":4660,"kind":"fold","level":1,"agent":"ai_b",
                "refs":[4661,4915],"title":"t","body":"b","content":"[fold 8132 · 2 refs · L1] \"t\""}"#,
        )
        .unwrap();
        assert_eq!(fold.refs.as_ref().unwrap().len(), 2);
        assert_eq!(fold.level, Some(1));
        assert!(
            fold.is_conversation(),
            "a fold IS conversation — it replaces what it covers"
        );

        let view: Row = serde_json::from_str(
            r#"{"v":1,"ts":1.0,"budget":14976,"kind":"view","id":8568,"agent":"ai_b",
                "why":"projection: fold","ids":[{"id":4660,"res":"full"},{"id":7065,"res":"stub"}]}"#,
        )
        .unwrap();
        assert_eq!(view.ids.as_ref().unwrap()[1].res, "stub");
        assert!(!view.is_conversation());
    }

    #[test]
    fn unknown_kind_survives_as_conversation() {
        // A kind this build has never heard of is rendered, not dropped.
        let r: Row =
            serde_json::from_str(r#"{"id":1,"agent":"a","kind":"telepathy","content":"x"}"#)
                .unwrap();
        assert!(r.is_conversation());
        assert_eq!(r.kind.as_deref(), Some("telepathy"));
    }

    #[test]
    fn unknown_field_survives_in_extra() {
        let r: Row = serde_json::from_str(r#"{"id":1,"agent":"a","tomorrow":{"k":2}}"#).unwrap();
        assert!(r.extra.contains_key("tomorrow"));
    }

    #[test]
    fn errored_status_parses_and_failed_is_a_status() {
        let o: Outcome =
            serde_json::from_str(r#"{"status":"errored","error":"no such path"}"#).unwrap();
        assert!(o.failed());
        let o: Outcome = serde_json::from_str(r#"{"status":"completed"}"#).unwrap();
        assert!(!o.failed());
        // A sixth status must not break anything, and must count as failure.
        let o: Outcome = serde_json::from_str(r#"{"status":"interrupted"}"#).unwrap();
        assert!(o.failed());
    }

    #[test]
    fn refusal_line_shows_the_kernels_own_hint() {
        let r: Refusal = serde_json::from_str(
            r#"{"error":"unauthorized: user -> exec start","reason":"unauthorized",
                "hint":"name the verb in can_call","side":"egress"}"#,
        )
        .unwrap();
        assert!(r.line().contains("name the verb"));
    }

    #[test]
    fn tree_walk_is_parents_before_children() {
        let t: TreeNode = serde_json::from_str(
            r#"{"id":"root","children":[{"id":"a","children":[{"id":"a1","children":[]}]},{"id":"b","children":[]}]}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        t.walk(0, &mut out);
        let ids: Vec<_> = out.iter().map(|(n, d)| (n.id.as_str(), *d)).collect();
        assert_eq!(ids, vec![("root", 0), ("a", 1), ("a1", 2), ("b", 1)]);
    }
}
