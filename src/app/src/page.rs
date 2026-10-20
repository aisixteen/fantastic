//! WHICH TOP-LEVEL URLS THE WINDOW MAY SHOW: the app's own origin (the boot
//! page), the home kernel's own port on `127.0.0.1` (the one origin the Home
//! page's capability names), and the ports of the canvases the home kernel
//! runs on `127.0.0.1` (the Home page's canvas frames). WebKitGTK passes
//! subframe navigations to this filter too, with no frame information, so the
//! frames are admitted by port here. Pure.

use serde_json::Value;
use tauri::Url;

/// `tauri:` (macOS, Linux), `tauri.localhost` (the app's origin on Windows), and
/// `http` on `127.0.0.1` at `home_port` once the kernel serves (`None` before),
/// or at one of `canvas_ports`, the running canvases' ports. Anything else is
/// refused.
pub fn allowed(url: &Url, home_port: Option<u16>, canvas_ports: &[u16]) -> bool {
    match url.scheme() {
        "tauri" => true,
        "http" | "https" if url.host_str() == Some("tauri.localhost") => true,
        "http" => {
            url.host_str() == Some("127.0.0.1")
                && url
                    .port_or_known_default()
                    .is_some_and(|p| home_port == Some(p) || canvas_ports.contains(&p))
        }
        _ => false,
    }
}

/// The ports of the canvases a home `state` payload says are running: each row's
/// `canvas` is `{url, port}` while it runs and `null` otherwise, so only the
/// running rows contribute. A port that is not one (zero, or past 65535) is
/// skipped.
pub fn running_canvas_ports(state: &Value) -> Vec<u16> {
    state["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["canvas"]["port"].as_u64())
        .filter_map(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Admitted with the home on 8123 and one running canvas on 8200.
    fn ok(u: &str) -> bool {
        allowed(&Url::parse(u).expect(u), Some(8123), &[8200])
    }

    #[test]
    fn the_apps_origin_and_the_homes_port_are_admitted() {
        assert!(ok("tauri://localhost/index.html"));
        assert!(ok("http://tauri.localhost/index.html"));
        assert!(ok("https://tauri.localhost/"));
        assert!(ok("http://127.0.0.1:8123/canvas_page/file/home.html"));
    }

    #[test]
    fn a_running_canvas_port_is_admitted() {
        assert!(ok("http://127.0.0.1:8200/canvas_page/file/index.html"));
        assert!(ok("http://127.0.0.1:8200/"));
        let two = [8200, 8201];
        let url = Url::parse("http://127.0.0.1:8201/").unwrap();
        assert!(allowed(&url, Some(8123), &two), "any running canvas");
    }

    #[test]
    fn a_stopped_or_unknown_canvas_port_is_refused() {
        assert!(!ok("http://127.0.0.1:8300/"), "no such canvas");
        let url = Url::parse("http://127.0.0.1:8200/").unwrap();
        assert!(!allowed(&url, Some(8123), &[]), "the canvas has stopped");
    }

    #[test]
    fn another_port_and_the_home_before_it_serves_are_refused() {
        assert!(!ok("http://127.0.0.1:1/"));
        assert!(!ok("http://127.0.0.1/"));
        let home = Url::parse("http://127.0.0.1:8123/canvas_page/file/home.html").unwrap();
        assert!(!allowed(&home, None, &[]), "no kernel serves yet");
        let canvas = Url::parse("http://127.0.0.1:8200/").unwrap();
        assert!(!allowed(&canvas, None, &[]), "no canvas is known yet");
        let boot = Url::parse("tauri://localhost/index.html").unwrap();
        assert!(
            allowed(&boot, None, &[]),
            "the boot page shows before the boot"
        );
    }

    #[test]
    fn anything_else_is_refused() {
        assert!(!ok("https://example.com"));
        assert!(!ok("http://localhost:8123/"));
        assert!(!ok("http://localhost:8200/"), "a canvas port, another host");
        assert!(!ok("https://127.0.0.1:8123/"));
        assert!(!ok("https://127.0.0.1:8200/"));
        assert!(!ok("http://127.0.0.2:8123/"));
        assert!(!ok("http://127.0.0.2:8200/"));
        assert!(!ok("file:///etc/passwd"));
        assert!(!ok("http://tauri.localhost.example.com/"));
    }

    #[test]
    fn the_running_canvas_ports_come_from_the_running_rows() {
        let state = json!({
            "type": "state",
            "rows": [
                {"id": "g-1", "canvas": {"url": "http://127.0.0.1:8200/", "port": 8200}},
                {"id": "g-2", "canvas": null},
                {"id": "adding:/x", "canvas": null},
                {"id": "g-3", "canvas": {"url": "http://127.0.0.1:8201/", "port": 8201}},
            ],
        });
        assert_eq!(running_canvas_ports(&state), [8200, 8201]);
    }

    #[test]
    fn a_state_without_running_canvases_has_no_ports() {
        assert!(running_canvas_ports(&json!({"type": "state", "rows": []})).is_empty());
        assert!(running_canvas_ports(&json!({"type": "state"})).is_empty());
        assert!(running_canvas_ports(&json!({"rows": [{"canvas": null}]})).is_empty());
    }

    #[test]
    fn a_canvas_without_a_real_port_is_skipped() {
        let state = json!({"rows": [
            {"canvas": {"url": "http://127.0.0.1:0/", "port": 0}},
            {"canvas": {"url": "http://127.0.0.1:70000/", "port": 70000}},
            {"canvas": {"url": "http://127.0.0.1:8200/", "port": "8200"}},
            {"canvas": {"url": "http://127.0.0.1:8202/"}},
            {"canvas": {"url": "http://127.0.0.1:8203/", "port": 8203}},
        ]});
        assert_eq!(running_canvas_ports(&state), [8203]);
    }
}
