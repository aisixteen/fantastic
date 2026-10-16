//! `realpath` of a RELATIVE path absolutizes against the cwd, and python's
//! `os.getcwd()` raises when the cwd is gone — measured:
//! `os.path.realpath('x')` in a removed dir is `FileNotFoundError: [Errno 2] No
//! such file or directory`. The error is propagated, never defaulted to `""`.
//!
//! Its own test binary: it changes the process cwd.
//!
//! POSIX only: Windows refuses to remove a directory some process has as its
//! cwd, so a cwd that is gone does not exist there.
#![cfg(unix)]

use std::path::Path;

#[test]
fn a_relative_path_under_a_removed_cwd_raises_getcwds_error() {
    let home = std::env::current_dir().unwrap();
    let gone = std::env::temp_dir().join(format!("realpath_cwd_{}", std::process::id()));
    std::fs::create_dir_all(&gone).unwrap();
    std::env::set_current_dir(&gone).unwrap();
    std::fs::remove_dir(&gone).unwrap();
    let got = fantastic_bundle::realpath(Path::new("x"));
    std::env::set_current_dir(&home).unwrap();
    assert_eq!(got, Err("[Errno 2] No such file or directory".to_string()));
    // an absolute path never reads the cwd
    assert_eq!(
        fantastic_bundle::realpath(Path::new("/a/./b/../c")),
        Ok(Path::new("/a/c").to_path_buf())
    );
}
