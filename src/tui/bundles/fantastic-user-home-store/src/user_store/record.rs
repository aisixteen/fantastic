//! The fields a user store's record carries, beside the ones every record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;

/// A user store's own record fields.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// The switch: `true` keeps this fantastic's folder in the home on every start; absent or `false`, off.
    pub register_home_folder: Option<bool>,
    /// This fantastic's folder name in the home; absent, its guid.
    pub home_folder_name: Option<String>,
}

fantastic_kernel::record!(Record);
