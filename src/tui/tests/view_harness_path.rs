//! The view harness a product's node tests import stays where `VIEW_HARNESS` says, beside
//! this crate's manifest, and exports what they use.

#[test]
fn the_view_harness_is_at_its_published_path_and_exports_its_runner() {
    let at = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(fantastic_tui::VIEW_HARNESS);
    let src = std::fs::read_to_string(&at).unwrap_or_else(|e| panic!("{}: {e}", at.display()));
    for name in ["export function run(", "export async function clocksReadNothing(", "export async function ownCopiesReadNothing("] {
        assert!(src.contains(name), "{} lacks {name}", at.display());
    }
}
