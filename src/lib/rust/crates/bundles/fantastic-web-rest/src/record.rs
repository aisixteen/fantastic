//! The fields a `web_rest` agent's record carries, beside the ones every record has
//! (issue #642).

use schemars::JsonSchema;

/// The fields a web_rest leg's record carries.
#[derive(Debug, JsonSchema)]
pub struct Record {
    /// Who may reach this leg; absent, the leg is sealed.
    pub ingress_rule: Option<fantastic_io_bridge::authorizer::IngressName>,
    /// The env var the `password_ingress` rule reads the group token from.
    #[schemars(length(min = 1))]
    pub token_env: Option<String>,
}

fantastic_kernel::record!(Record);
