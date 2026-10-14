//! The blob channel — name validation. WHO may name a namespace is the grant
//! alone (owner, 2026-10-06): an agent that reaches `persist_blob` names any.

use super::*;

#[test]
fn a_blob_name_must_be_one_plain_filename() {
    assert_eq!(guard_name(Some("state.yaml")).unwrap(), "state.yaml");

    // Path segments would let a caller climb out of its own namespace.
    for bad in ["a/b.yaml", "..\\x", "sub/dir/f"] {
        let e = guard_name(Some(bad)).unwrap_err();
        assert!(
            e.contains("single filename") || e.contains("hidden"),
            "{bad}: {e}"
        );
    }
    // Hidden / traversal.
    assert!(guard_name(Some(".secret")).unwrap_err().contains("hidden"));
    assert!(guard_name(Some("a..b")).unwrap_err().contains("hidden"));
    // Absent.
    assert!(guard_name(None).unwrap_err().contains("required"));
    assert!(guard_name(Some("")).unwrap_err().contains("required"));
}

#[test]
fn kernel_managed_files_are_reserved() {
    // A sidecar that could overwrite `agent.yaml` would let a consumer rewrite
    // its own record — including its grants — through a byte channel.
    for reserved in ["agent.yaml", "AGENT.YAML"] {
        let e = guard_name(Some(reserved)).unwrap_err();
        assert!(e.contains("reserved"), "{reserved}: {e}");
    }
}
