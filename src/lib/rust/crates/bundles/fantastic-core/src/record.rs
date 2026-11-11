//! The fields a `kernel_state.tools` agent's record carries, beside the ones every
//! record has (issue #642). The ROOT's record is the one a product seeds with its own
//! fields (`workdir`, a blueprint's root fields, …), so it is marked OPEN: any field
//! is accepted.

use schemars::JsonSchema;
use serde::Deserialize;

/// The loader's own record fields (the record is open: a product's root fields ride it).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// A SESSION loader serves a sub-namespace: its disk folder (`@state`, `@self`, or a path); absent, its own address.
    pub root: Option<String>,
    /// `false` runs no auto-flush loop: the loader serves only the verbs (a page kernel's loader).
    pub watch: Option<bool>,
}

fantastic_kernel::record!(Record, open);
