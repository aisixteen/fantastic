//! Unix (macOS, Linux): libc, and python's `posixpath` grammar.

/// python `posixpath.expanduser` of a path starting `~`: `~` is `$HOME` (the
/// password database when unset), `~user` that user's home; `None` when there
/// is no home to expand to.
pub(crate) fn expanduser(p: &str) -> Option<String> {
    let i = p[1..].find('/').map_or(p.len(), |j| j + 1);
    let home = if i == 1 {
        match std::env::var("HOME") {
            Ok(h) => Some(h),
            Err(_) => home_of(None),
        }
    } else {
        home_of(Some(&p[1..i]))
    }?;
    let trimmed = home.trim_end_matches('/');
    let out = format!("{trimmed}{}", &p[i..]);
    Some(if out.is_empty() { "/".to_string() } else { out })
}

/// pathlib's split of a path into (parent, name): empty and `.` parts dropped,
/// `..` kept, the parent of a bare name `.` (or `/` under the root).
pub(crate) fn split_path(p: &str) -> (String, String) {
    let absolute = p.starts_with('/');
    let mut parts: Vec<&str> = p
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let name = parts.pop().unwrap_or("").to_string();
    let joined = parts.join("/");
    let parent = match (absolute, joined.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    };
    (parent, name)
}

/// The password database's home directory for `user` (this uid when `None`).
fn home_of(user: Option<&str>) -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let rc = match user {
        None => {
            // SAFETY: every pointer is valid for the call; `buf` outlives it.
            unsafe {
                libc::getpwuid_r(
                    libc::getuid(),
                    &mut pwd,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &mut out,
                )
            }
        }
        Some(name) => {
            let name = std::ffi::CString::new(name).ok()?;
            // SAFETY: as above; `name` is NUL-terminated and outlives the call.
            unsafe {
                libc::getpwnam_r(
                    name.as_ptr(),
                    &mut pwd,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &mut out,
                )
            }
        }
    };
    if rc != 0 || out.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    // SAFETY: `pw_dir` points into `buf`, NUL-terminated by the C library.
    let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
    Some(dir.to_string_lossy().into_owned())
}
