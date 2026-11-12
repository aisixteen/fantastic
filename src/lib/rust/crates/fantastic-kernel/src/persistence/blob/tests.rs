//! The blob channel — the one rule that needs the kernel's list. A name's SHAPE and
//! an agent id's SHAPE are the verb declaration's patterns (tested with the loader's
//! verbs in `fantastic-core`). WHO may name a namespace is the grant alone.

use super::*;

#[test]
fn kernel_managed_files_are_reserved() {
    // A sidecar that could overwrite `agent.yaml` would let a consumer rewrite
    // its own record — including its grants — through a byte channel.
    for reserved in ["agent.yaml", "AGENT.YAML"] {
        assert!(reserved_name(reserved), "{reserved}");
    }
    assert!(!reserved_name("state.yaml"));
}
