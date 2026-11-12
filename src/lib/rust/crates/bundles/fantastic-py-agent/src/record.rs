//! The fields a py_agent instance's record carries, beside the ones every record has
//! (issue #642). A bundle's `bundle.yaml` `record:` lays its own defaults under each
//! instance, and the program reads them, so the record is OPEN: any field is
//! accepted.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

/// The record fields the base itself reads (the record is open: the program's own fields ride it).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// Names of the kernel's environment variables the program gets (`["*"]` = every one); it gets nothing else.
    pub inherit_env: Option<Vec<String>>,
    /// Environment variables set for the program, over what it inherits.
    pub env: Option<Map<String, Value>>,
}

fantastic_kernel::record!(Record, open);
