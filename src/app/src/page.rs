//! WHICH TOP-LEVEL URLS THE WINDOW MAY SHOW: the app's own origin (the boot
//! page) and the home kernel's own port on `127.0.0.1` — the one origin the
//! Home page's capability names. Canvas frames inside the Home page are not
//! top-level navigations. Pure.

use tauri::Url;

/// `tauri:` (macOS, Linux), `tauri.localhost` (the app's origin on Windows), and
/// `http` on `127.0.0.1:<home_port>` once the kernel serves (`None` before).
/// Anything else is refused.
pub fn allowed(url: &Url, home_port: Option<u16>) -> bool {
    match url.scheme() {
        "tauri" => true,
        "http" | "https" if url.host_str() == Some("tauri.localhost") => true,
        "http" => {
            url.host_str() == Some("127.0.0.1") && home_port.is_some() && url.port() == home_port
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(u: &str) -> bool {
        allowed(&Url::parse(u).expect(u), Some(8123))
    }

    #[test]
    fn the_apps_origin_and_the_homes_port_are_admitted() {
        assert!(ok("tauri://localhost/index.html"));
        assert!(ok("http://tauri.localhost/index.html"));
        assert!(ok("https://tauri.localhost/"));
        assert!(ok("http://127.0.0.1:8123/canvas_page/file/home.html"));
    }

    #[test]
    fn another_port_and_the_home_before_it_serves_are_refused() {
        assert!(!ok("http://127.0.0.1:1/"));
        assert!(!ok("http://127.0.0.1/"));
        let home = Url::parse("http://127.0.0.1:8123/canvas_page/file/home.html").unwrap();
        assert!(!allowed(&home, None), "no kernel serves yet");
        let boot = Url::parse("tauri://localhost/index.html").unwrap();
        assert!(allowed(&boot, None), "the boot page shows before the boot");
    }

    #[test]
    fn anything_else_is_refused() {
        assert!(!ok("https://example.com"));
        assert!(!ok("http://localhost:8123/"));
        assert!(!ok("https://127.0.0.1:8123/"));
        assert!(!ok("http://127.0.0.2:8123/"));
        assert!(!ok("file:///etc/passwd"));
        assert!(!ok("http://tauri.localhost.example.com/"));
    }
}
