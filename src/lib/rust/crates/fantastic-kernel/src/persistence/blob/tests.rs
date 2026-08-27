//! The blob channel — name validation and the OWNER guard.
//!
//! The guard is the point: `agent_id` is the namespace and it arrives in the
//! PAYLOAD, so without it any agent granted `persist_blob` could write any
//! other agent's sidecar.

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
    // A sidecar that could overwrite `agent.json` would let a consumer rewrite
    // its own record — including its grants — through a byte channel.
    for reserved in ["agent.json", "AGENT.JSON", "readme.md"] {
        let e = guard_name(Some(reserved)).unwrap_err();
        assert!(e.contains("reserved"), "{reserved}: {e}");
    }
}

#[tokio::test]
async fn an_operator_may_name_any_namespace() {
    // Origin `None` is the bookkeeping this verb exists for: the loader's own
    // flush, an agent persisting itself under operator authority, the CLI.
    assert!(guard_owner("anyone").is_ok());
}

#[tokio::test]
async fn an_agent_may_only_name_itself() {
    // The measured breach (python, 2026-08-23): an agent holding exactly
    // `can_call: {loader: [persist_blob]}` overwrote ANOTHER agent's state.yaml,
    // and the victim read the forged content back as its own memory.
    crate::send::CURRENT_ORIGIN
        .scope(Some(AgentId::from("alice")), async {
            assert!(guard_owner("alice").is_ok(), "an agent must reach its own");

            let e = guard_owner("bob").unwrap_err();
            assert!(e.contains("alice"), "{e}");
            assert!(e.contains("bob"), "{e}");
            assert!(e.contains("scoped to itself"), "{e}");
        })
        .await;
}
