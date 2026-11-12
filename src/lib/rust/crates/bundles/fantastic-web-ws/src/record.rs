//! The fields a `web_ws` agent's record carries, beside the ones every record has
//! (issue #642).

use schemars::JsonSchema;

/// The fields a web_ws leg's record carries.
#[derive(Debug, JsonSchema)]
pub struct Record {
    /// A literal mount path such as `/page_ws/ws` (starts with `/`, no `{` or `}`): the leg then mounts only that path; absent, the host's catch-all.
    #[schemars(length(min = 1), regex(pattern = r"^/[^{}]*$"))]
    pub route: Option<String>,
    /// Who may reach this leg; absent, the leg is sealed.
    pub ingress_rule: Option<fantastic_io_bridge::authorizer::IngressName>,
    /// The env var the `password_ingress` rule reads the group token from.
    #[schemars(length(min = 1))]
    pub token_env: Option<String>,
}

fantastic_kernel::record!(Record);
