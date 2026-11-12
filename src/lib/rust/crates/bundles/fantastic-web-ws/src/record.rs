//! The fields a `web_ws` agent's record carries, beside the ones every record has
//! (issue #642).

use schemars::JsonSchema;
use serde_json::Value;

/// The fields a web_ws leg's record carries.
#[derive(Debug, JsonSchema)]
pub struct Record {
    /// A literal mount path such as `/page_ws/ws` (starts with `/`, no `{` or `}`): the leg then mounts only that path; absent, the host's catch-all.
    #[schemars(length(min = 1), regex(pattern = r"^/[^{}]*$"))]
    pub route: Option<String>,
    /// Who may reach this leg: a rule name (`allow_all_ingress`, `deny_ingress`, `password_ingress`) or `{type, env}`; absent, the leg is sealed.
    pub ingress_rule: Option<Value>,
    /// The shorthand stem a rule name is built from per direction (`password`).
    pub auth: Option<Value>,
}

fantastic_kernel::record!(Record);
