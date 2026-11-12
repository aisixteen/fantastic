//! What the test stand-ins declare: the verbs they really answer, as the bundles they
//! stand in for do (a stand-in chat answers the chat's verbs, with an open record).

use schemars::JsonSchema;

/// A record with no declared fields, open: whatever a test seeds rides on it.
pub mod any {
    use super::JsonSchema;

    /// The stand-ins' record: freeform.
    #[derive(JsonSchema)]
    pub struct AnyRecord {}

    fantastic_kernel::record!(AnyRecord, open);
}

/// The declarations a stand-in CHAT carries inside its `impl Handler`.
macro_rules! chat_decls {
    () => {
        type Call = $crate::declared::chat::Call;
        fn man(&self) -> &'static str {
            "A stand-in chat for a test."
        }
        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            $crate::declared::chat::VERBS
        }
        fn dynamic_verbs(&self, a: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            $crate::declared::chat::dynamic_verbs(a)
        }
        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            &$crate::tests_decl::any::RECORD
        }
    };
}
pub(crate) use chat_decls;


/// What a write of `meta` to a chat's record answers: the engine's refusal of a field
/// (its name, type, range or pattern). `None`: the write is taken.
pub fn refused_at_the_write(meta: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    fantastic_kernel::verbs::check_record(
        "update_agent",
        "ai_chat",
        meta,
        &fantastic_kernel::declared::record::RECORD,
        &crate::chat_bundle::RECORD,
    )
    .err()
    .map(|refused| fantastic_kernel::refusal::error_text(&refused).unwrap_or_default())
}
