//! The blob channel — name validation. WHO may name a namespace is the grant
//! alone: an agent that reaches `persist_blob` names any.

use super::*;

#[test]
fn a_blob_name_must_be_one_plain_filename() {
    assert_eq!(guard_name("state.yaml").unwrap(), "state.yaml");

    // Path segments would let a caller climb out of its own namespace.
    for bad in ["a/b.yaml", "..\\x", "sub/dir/f"] {
        let e = guard_name(bad).unwrap_err();
        assert!(
            e.contains("single filename") || e.contains("hidden"),
            "{bad}: {e}"
        );
    }
    // Hidden / traversal.
    assert!(guard_name(".secret").unwrap_err().contains("hidden"));
    assert!(guard_name("a..b").unwrap_err().contains("hidden"));
    // Empty.
    assert!(guard_name("").unwrap_err().contains("non-empty"));
}

#[test]
fn kernel_managed_files_are_reserved() {
    // A sidecar that could overwrite `agent.yaml` would let a consumer rewrite
    // its own record — including its grants — through a byte channel.
    for reserved in ["agent.yaml", "AGENT.YAML"] {
        let e = guard_name(reserved).unwrap_err();
        assert!(e.contains("reserved"), "{reserved}: {e}");
    }
}

#[test]
fn an_agent_id_that_walks_out_is_refused() {
    // `agents/<agent_id>/` is a directory: `../bundles/x` would reach a folder
    // bundle's `bundle.yaml`.
    for verb in ["persist_blob", "load_blob"] {
        for bad in ["../bundles/x", "a/b", "..", ".hidden"] {
            let e = guard_args(verb, bad, "bundle.yaml").expect_err(bad);
            assert_eq!(e["arg"], "agent_id", "{verb} {bad}: {e}");
        }
    }
}
