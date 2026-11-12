//! The verbs each scenario fixture declares (issue #642): one module per probe, each
//! with one struct per verb and its `verbs!` — the list the probe's own entry
//! (`Call::parse`) checks every call against, and that `reflect` and `docs` render.
//! `reflect` is declared once, in the kernel.

/// `dispenser_probe`.
pub mod dispenser {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// Issue ONE coin; returns {coin, receipt, job}. Issuing is a side effect: calling twice dispenses twice.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Dispense {
        /// The job the coin is for.
        pub job: Option<String>,
    }

    /// How many coins have ever been dispensed. Returns {count}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Count {}

    fantastic_kernel::verbs! {
        "dispense" => Dispense,
        "count" => Count,
    }
}

/// `plant_probe`.
pub mod plant {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// Run the pump. It fails intermittently.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Pump {}

    /// Operate the valve.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Valve {}

    /// How many times the plant has been called. Returns {count}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Count {}

    fantastic_kernel::verbs! {
        "pump" => Pump,
        "valve" => Valve,
        "count" => Count,
    }
}

/// `purchase_probe`.
pub mod purchase {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// Buy ONE; returns {bought, receipt, n}. Spending is a side effect.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Buy {
        /// What to buy.
        pub item: Option<String>,
    }

    /// How many purchases have ever been made. Returns {count}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Count {}

    fantastic_kernel::verbs! {
        "buy" => Buy,
        "count" => Count,
    }
}

/// `user_probe`.
pub mod user {
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::{Map, Value};

    fn cli() -> String {
        "cli".to_string()
    }

    /// The four shapes a UI can render. Advisory for this human: it answers the same way regardless.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "lowercase")]
    pub enum Kind {
        /// Free text.
        Text,
        /// One option.
        Single,
        /// Several options.
        Multi,
        /// Yes or no.
        Confirm,
    }

    /// Ask the human. ACCEPTED at once ({accepted, re, request_id, inquiries, granted}); the answer arrives LATER as this agent's message under the inquiry's pid. Nothing suspends.
    ///
    /// An `action` is a PROPOSAL the sender may not perform itself: it is HELD, never run here.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Inquiry {
        /// The question.
        #[schemars(length(min = 1), regex(pattern = "\\S"))]
        pub text: String,
        /// The shape the UI renders; `text` when absent.
        pub kind: Option<Kind>,
        /// The choices for `single` and `multi`.
        pub options: Option<Vec<Value>>,
        /// A proposal the sender may not perform itself.
        pub action: Option<Map<String, Value>>,
        /// Names this inquiry in the answer.
        pub request_id: Option<String>,
        /// The client the answer is addressed to.
        #[serde(default = "cli")]
        pub reply_client_id: String,
    }

    /// How many inquiries this agent has received, and every `action` one handed it. Returns {inquiries, proposals}: the tally, not prose.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Inquiries {}

    /// Dispatch ANY payload to ANY agent as THIS agent: work, an inquiry of its own, a kernel verb. Gated like any send; the ledger row names this agent as the sender.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Send {
        /// The agent to send to.
        #[schemars(length(min = 1))]
        pub target: String,
        /// The call: an object naming a `type`.
        #[schemars(schema_with = "call_with_type")]
        pub payload: Map<String, Value>,
    }

    /// The schema of a sent call: an object whose `type` is text.
    fn call_with_type(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "required": ["type"],
            "properties": {
                "type": {"type": "string", "minLength": 1, "description": "The verb the target answers."}
            }
        }))
        .expect("a call schema")
    }

    fantastic_kernel::verbs! {
        "inquiry" => Inquiry,
        "inquiries" => Inquiries,
        "send" => Send,
    }
}

/// `oracle_probe`.
pub mod oracle {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// Keyword search over my ledger: the lines matching the most words of the question come back as {answers:[str], matched:int}. Ask precisely; a vague question returns noise.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Ask {
        /// The question; every word of three letters or more is a keyword.
        #[schemars(length(min = 1), regex(pattern = "\\S"))]
        pub question: String,
        /// How many lines at most; 0 means the default six, otherwise clamped to 1..20.
        #[serde(default = "six")]
        pub limit: i64,
    }

    fn six() -> i64 {
        6
    }

    fantastic_kernel::verbs! {
        "ask" => Ask,
    }
}

/// `slow_probe`.
pub mod slow {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// Sleep for an hour: in flight until cancelled.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Slow {}

    /// Count one run. Returns {runs}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Work {}

    /// Raise: a failed execution, a dead letter.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Fail {}

    /// How many `slow` calls have started. Returns {started}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Started {}

    /// Sleep that long, then return {slept}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Sleep {
        /// How long, in seconds; not negative.
        #[schemars(range(min = 0))]
        pub seconds: serde_json::Number,
    }

    fantastic_kernel::verbs! {
        "slow" => Slow,
        "work" => Work,
        "fail" => Fail,
        "started" => Started,
        "sleep" => Sleep,
    }
}

/// `vault_probe`.
pub mod vault {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// Open the vault and return {secret}.
    ///
    /// THE CODE: this vault's `serial` (its reflect shows it) written BACKWARDS and prefixed with `V-`. Any other code is refused `bad_arg`, and the vault stays shut.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Open {
        /// The code that opens the vault.
        pub code: String,
    }

    /// How many times this vault has been opened. Returns {opened}.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Count {}

    fantastic_kernel::verbs! {
        "open" => Open,
        "count" => Count,
    }
}
