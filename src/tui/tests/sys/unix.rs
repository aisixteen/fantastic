//! The Unix side of the tests' `sys`.

/// The command and args of a program that prints `text`.
pub fn echo(text: &str) -> (String, Vec<String>) {
    ("echo".into(), vec![text.into()])
}
