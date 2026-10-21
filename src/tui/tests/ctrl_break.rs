//! `fantastic __ctrl_break <pid>`: the hidden stop helper the home kernel runs on
//! Windows. Driven through the built binary itself.

use std::process::Command;

fn run(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fantastic"))
        .args(args)
        .output()
        .expect("the fantastic binary runs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[cfg(unix)]
#[test]
fn ctrl_break_is_windows_only() {
    let (code, stderr) = run(&["__ctrl_break", "123"]);
    assert_eq!(code, Some(2), "stderr: {stderr}");
    assert!(stderr.contains("Windows only"), "stderr: {stderr}");
}

#[cfg(windows)]
#[test]
fn ctrl_break_names_the_failed_step() {
    // This test runs in a console, so the helper is attached to one already and
    // `AttachConsole` refuses.
    let (code, stderr) = run(&["__ctrl_break", "4294967295"]);
    assert_eq!(code, Some(1), "stderr: {stderr}");
    assert!(stderr.contains("AttachConsole"), "stderr: {stderr}");
    let (code, stderr) = run(&["__ctrl_break", "not-a-pid"]);
    assert_eq!(code, Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("usage: fantastic __ctrl_break <pid>"),
        "stderr: {stderr}"
    );
}
