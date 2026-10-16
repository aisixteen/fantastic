//! The Windows side of the tests' `sys`.

/// The command and args of a program that prints `text`: `echo` is a builtin of
/// `cmd.exe`, not a program of its own.
pub fn echo(text: &str) -> (String, Vec<String>) {
    ("cmd".into(), vec!["/c".into(), "echo".into(), text.into()])
}
