//! The parser's values, READ AS THE TARGET VERB'S SCHEMA DECLARES.
//!
//! The grammar reads text: a leaf is the string the model wrote. [`payload`] reads a
//! call's payload against the declared schema of the verb it names (a builtin verb of
//! the target's bundle, a dynamic verb of that agent, or one of the kernel's own) and
//! produces each value in the declared type — or refuses the call by argument name; no
//! wrongly typed value goes on to the target. The reader is the kernel's
//! ([`fantastic_kernel::typing`]); the operator's `k=v` goes through the same code.

pub use fantastic_kernel::typing::payload;
use serde_json::Value;

/// The routing fields a call carries that are not text — how long to wait, how often
/// to retry — read as the type they are; every other routing field is text. `Err` is the
/// refusal naming the field.
pub fn routing(name: &str, v: &Value) -> Result<Value, Value> {
    match name {
        "timeout" => fantastic_kernel::typing::routing(name, "number", v),
        "retries" => fantastic_kernel::typing::routing(name, "integer", v),
        _ => Ok(v.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_timeout_and_retries_are_read_as_numbers_in_routing() {
        assert_eq!(routing("timeout", &json!("2.5")).unwrap(), json!(2.5));
        assert_eq!(routing("retries", &json!("3")).unwrap(), json!(3));
        assert_eq!(routing("reply_to", &json!("1001")).unwrap(), json!("1001"));
        assert_eq!(routing("timeout", &json!("soon")).unwrap_err()["arg"], json!("timeout"));
    }
}
