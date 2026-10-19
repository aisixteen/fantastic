//! THE SHELL'S FLAG: the initialization script that sets
//! `globalThis.__FANTASTIC_SHELL__` before any page script runs (read by the
//! page's `shell.ts`, `readShell()`). Pure.
//!
//! `initialization_script` runs in the main frame only, on every navigation:
//! canvas frames get no flag.

/// The script for `os`, `webview` and `backdrop` (`native` | `web`); with
/// `folder_dialog`, `pickFolder` invokes the shell's `pick_folders`.
pub fn script(os: &str, webview: &str, backdrop: &str, folder_dialog: bool) -> String {
    let j = |s: &str| serde_json::to_string(s).expect("a string serializes");
    let pick = if folder_dialog {
        format!(
            "\n    pickFolder: () => globalThis.__TAURI_INTERNALS__.invoke({}),",
            j("pick_folders")
        )
    } else {
        String::new()
    };
    format!(
        "Object.defineProperty(globalThis, \"__FANTASTIC_SHELL__\", {{ value: Object.freeze({{\n    name: {}, os: {}, webview: {}, backdrop: {},{pick}\n  }}), writable: false, configurable: false }});\n",
        j("tauri"),
        j(os),
        j(webview),
        j(backdrop),
    )
}

#[cfg(test)]
mod tests {
    use super::script;

    #[test]
    fn the_four_fields_with_valid_values() {
        for (os, webview) in [
            ("macos", "wkwebview"),
            ("windows", "webview2"),
            ("linux", "webkitgtk"),
        ] {
            for backdrop in ["native", "web"] {
                let s = script(os, webview, backdrop, false);
                assert!(
                    s.starts_with("Object.defineProperty(globalThis, \"__FANTASTIC_SHELL__\""),
                    "{s}"
                );
                assert!(s.contains("name: \"tauri\""), "{s}");
                assert!(s.contains(&format!("os: \"{os}\"")), "{s}");
                assert!(s.contains(&format!("webview: \"{webview}\"")), "{s}");
                assert!(s.contains(&format!("backdrop: \"{backdrop}\"")), "{s}");
                assert!(s.contains("Object.freeze("), "{s}");
                assert!(s.contains("writable: false, configurable: false"), "{s}");
            }
        }
    }

    #[test]
    fn pick_folder_only_when_asked() {
        assert!(!script("linux", "webkitgtk", "web", false).contains("pickFolder"));
        let s = script("macos", "wkwebview", "native", true);
        assert!(
            s.contains(
                "pickFolder: () => globalThis.__TAURI_INTERNALS__.invoke(\"pick_folders\"),"
            ),
            "{s}"
        );
    }

    #[test]
    fn values_are_json_escaped() {
        let s = script("a\"b", "c\\d", "e\n", false);
        assert!(s.contains(r#"os: "a\"b""#), "{s}");
        assert!(s.contains(r#"webview: "c\\d""#), "{s}");
        assert!(s.contains(r#"backdrop: "e\n""#), "{s}");
    }
}
