//! WHICH TOP-LEVEL URLS THE WINDOW MAY SHOW: the app's own origin (the boot
//! page) and the home kernel's loopback host. Canvas frames inside the Home
//! page are not top-level navigations. Pure.

use tauri::Url;

/// `tauri:` (macOS, Linux), `tauri.localhost` (the app's origin on Windows), and
/// `http` on `127.0.0.1`. Anything else is refused.
pub fn allowed(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => true,
        "http" | "https" if url.host_str() == Some("tauri.localhost") => true,
        "http" => url.host_str() == Some("127.0.0.1"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(u: &str) -> bool {
        allowed(&Url::parse(u).expect(u))
    }

    #[test]
    fn the_apps_origin_and_the_loopback_host_are_admitted() {
        assert!(ok("tauri://localhost/index.html"));
        assert!(ok("http://tauri.localhost/index.html"));
        assert!(ok("https://tauri.localhost/"));
        assert!(ok("http://127.0.0.1:8123/canvas_page/file/home.html"));
        assert!(ok("http://127.0.0.1:1/"));
    }

    #[test]
    fn anything_else_is_refused() {
        assert!(!ok("https://example.com"));
        assert!(!ok("http://localhost:1/"));
        assert!(!ok("https://127.0.0.1:8123/"));
        assert!(!ok("http://127.0.0.2:8123/"));
        assert!(!ok("file:///etc/passwd"));
        assert!(!ok("http://tauri.localhost.example.com/"));
    }
}
