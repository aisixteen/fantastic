//! The verbs `user` declares (issue #642): one struct per verb, then `verbs!` — the
//! list the agent's own entry checks every call against, and that `reflect` and `docs`
//! render. `reflect` is declared once, in the kernel; `export_subtree` is the root's
//! own declaration (`fantastic_core::verbs`), the same engine under `user`.

use schemars::JsonSchema;
use fantastic_kernel::inquiry::{Answer, Inquiries, Inquiry};
use serde::Deserialize;
use serde_json::{Map, Value};

pub use fantastic_core::verbs::ExportSubtree;

/// The kernel starts `user`: the scan of the human's programs begins, off the boot's path. Answers nothing.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// `user` sends `payload` to `target`; the reply comes back verbatim. No default timeout: the call lasts as long as the target takes. No secret marks: values travel and are recorded as they are.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Forward {
    /// The agent the call goes to.
    #[schemars(length(min = 1))]
    pub target: String,
    /// The call: an object whose `type` is the verb the target answers, with that verb's arguments.
    pub payload: Map<String, Value>,
    /// Seconds; none by default.
    pub timeout: Option<f64>,
}

/// The cells of a page view within `radius` of a point, nearest first, from the page records. Judged by the caller's own page grants: the shell sees everything; any other caller by its own `can_call` (`<tree guid>|<id>`). Returns {view, members:[{id, kind, title, rect:{x,y,w,h}, distance, backend_id?}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Around {
    /// A canvas node id; absent, the main canvas (this record's `page_loader`).
    #[schemars(length(min = 1))]
    pub canvas: Option<String>,
    /// The page view; `view_root` by default.
    #[schemars(length(min = 1))]
    pub view: Option<String>,
    /// The point's x.
    pub x: f64,
    /// The point's y.
    pub y: f64,
    /// How far from the point; more than zero.
    #[schemars(schema_with = "positive_number")]
    pub radius: f64,
}

/// The schema of a number greater than zero.
fn positive_number(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "number",
        "exclusiveMinimum": 0.0
    }))
    .expect("a number schema")
}

/// The human's programs: every executable on the PATH directories (this process's, merged with the login shell's), the first on PATH winning; `executables.fantastic` is always the running fantastic's own executable. Scanned when `user` boots and kept. Returns {dirs, executables:{name: full path}, self}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Path {
    /// `true` scans again.
    pub rescan: Option<bool>,
}

/// The person's answer, sent to the asker under the inquiry's pid; then the question is closed. Returns {answered, asker}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerInquiry {
    /// The inquiry's pid.
    pub re: i64,
    /// The answer.
    pub answer: Answer,
}

/// A map of port name to a non-empty agent id.
fn agent_ids(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": {"type": "string", "minLength": 1},
    }))
    .expect("a map schema")
}

/// The other half of `export_subtree`: the blueprint folder at `path` becomes agents under `parent_id`, every id fresh (one `_<k>` suffix), every port bound on both ends. Without `approve: true` it only shows what the team will reach and the code it carries ({approval_required, agents, new_ids, reaches, ports, code}) and nothing is created; with it, the agents are made and the bound agents granted back ({imported, ids, parent_id, ports}). Refused: unbound_ports, bad_binding, secret_found, code_not_allowed.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportSubtree {
    /// A blueprint folder, relative to the running dir (no `..`).
    #[schemars(length(min = 1))]
    pub path: String,
    /// The agent the imported team goes under.
    #[schemars(length(min = 1))]
    pub parent_id: String,
    /// Every declared port, bound to an agent of its kind: `{"@port": "<agent id>"}`.
    #[serde(default)]
    #[schemars(schema_with = "agent_ids")]
    pub bind: Option<std::collections::BTreeMap<String, String>>,
    /// `true` allows code (a handler that is not a registered bundle, or a file other than the blueprint and directives).
    pub allow_code: Option<bool>,
    /// `true` creates the agents; absent or `false`, the preview.
    pub approve: Option<bool>,
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "forward" => Forward,
    "around" => Around,
    "path" => Path,
    "inquiry" => Inquiry,
    "inquiries" => Inquiries,
    "answer_inquiry" => AnswerInquiry,
    "export_subtree" => ExportSubtree,
    "import_subtree" => ImportSubtree,
}
