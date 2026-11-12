//! The fields each scenario fixture's record carries, beside the ones every record
//! has (issue #642): the policy a scenario writes on the record, never code. A fixture
//! with no fields of its own has an empty record.

/// `dispenser_probe`'s record: no fields of its own.
pub mod dispenser {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// No fields of its own.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct Record {}
    fantastic_kernel::record!(Record);
}

/// `plant_probe`'s record: no fields of its own.
pub mod plant {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// No fields of its own.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct Record {}
    fantastic_kernel::record!(Record);
}

/// `purchase_probe`'s record: no fields of its own.
pub mod purchase {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// No fields of its own.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct Record {}
    fantastic_kernel::record!(Record);
}

/// `user_probe`'s record.
pub mod user {
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::{Map, Value};

    /// What the human says and does.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct Record {
        /// What the human answers to every inquiry; without it an inquiry is refused `not_connected`.
        pub answer: Option<String>,
        /// Grants the human applies, through the root, BEFORE answering: each one a call to the root.
        pub grants: Option<Vec<Map<String, Value>>>,
    }
    fantastic_kernel::record!(Record);
}

/// `oracle_probe`'s record.
pub mod oracle {
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::Value;

    /// What the oracle knows.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct Record {
        /// The ledger: one fact per line; without it `ask` is refused `not_connected`.
        pub lines: Option<Vec<Value>>,
    }
    fantastic_kernel::record!(Record);
}

/// `slow_probe`'s record: no fields of its own.
pub mod slow {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// No fields of its own.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct Record {}
    fantastic_kernel::record!(Record);
}

/// `vault_probe`'s record.
pub mod vault {
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::Value;

    /// What the vault holds.
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct Record {
        /// The vault's serial, shown on its reflect; the code is made from it.
        pub serial: Option<Value>,
        /// What opening the vault returns.
        pub secret: Option<Value>,
    }
    fantastic_kernel::record!(Record);
}
