//! The parser's values, TYPED FROM THE TARGET VERB'S SCHEMA.
//!
//! The grammar reads text: a leaf is the string the model wrote. Which type a
//! string stands for is the target verb's own declaration — a `path` of `007` is
//! the text `007`, a `limit` of `7` is the integer 7. [`payload`] walks a call's
//! payload against the declared schema of the verb it names (a builtin verb of the
//! target's bundle, a dynamic verb of that agent, or one of the kernel's own) and
//! produces each value in the type the schema states; a string where the schema
//! allows text stays text. A value that is not of the type it should be is left as
//! written: the target's own validation refuses it by name. The typing itself is
//! the kernel's ([`fantastic_kernel::typing`]); the operator's `k=v` goes through the
//! same code.

pub use fantastic_kernel::typing::{payload, typed};
use serde_json::Value;

/// The routing fields a call carries that are not text: how long to wait and how
/// often to retry. Every other routing field (`target_id`, `reply_to`, `title`,
/// `exec_id`, …) is text.
#[must_use]
pub fn routing(name: &str, v: &Value) -> Value {
    let schema = match name {
        "timeout" => serde_json::json!({"type": "number"}),
        "retries" => serde_json::json!({"type": "integer"}),
        _ => return v.clone(),
    };
    typed(&schema, v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_timeout_and_retries_are_typed_in_routing() {
        assert_eq!(routing("timeout", &json!("2.5")), json!(2.5));
        assert_eq!(routing("retries", &json!("3")), json!(3));
        assert_eq!(routing("reply_to", &json!("1001")), json!("1001"));
    }
}
