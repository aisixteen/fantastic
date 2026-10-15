//! Unix (macOS, Linux): libc.

/// The password database's home directory for `user` (this uid when `None`).
pub(crate) fn home_of(user: Option<&str>) -> Option<String> {
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
