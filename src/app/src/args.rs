//! THE ARGUMENTS: `fantastic-app [--kiosk] [<folder>…]`. Pure.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What the app was started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// The kiosk's screen: fullscreen, no decorations, no menu, no dialogs.
    pub kiosk: bool,
    /// The folders to open, absolute (a relative one joined to `cwd`).
    pub paths: Vec<PathBuf>,
}

/// `argv` (without the program) read against `cwd`. `--kiosk` is the only
/// option; any other word starting with `-` is refused. Every other word is a folder.
pub fn parse(argv: impl IntoIterator<Item = OsString>, cwd: &Path) -> Result<Args, String> {
    let mut args = Args {
        kiosk: false,
        paths: Vec::new(),
    };
    for word in argv {
        if word == "--kiosk" {
            args.kiosk = true;
        } else if word.as_encoded_bytes().starts_with(b"-") {
            return Err(format!("unknown option {}", word.to_string_lossy()));
        } else {
            args.paths.push(cwd.join(word));
        }
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str], cwd: &Path) -> Result<Args, String> {
        parse(words.iter().map(OsString::from), cwd)
    }

    fn root() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn kiosk_and_paths() {
        let cwd = root();
        let abs = cwd.join("abs");
        let got = parse_words(&["--kiosk", abs.to_str().unwrap()], &cwd).unwrap();
        assert_eq!(
            got,
            Args {
                kiosk: true,
                paths: vec![abs]
            }
        );
        let none = parse_words(&[], &cwd).unwrap();
        assert_eq!(
            none,
            Args {
                kiosk: false,
                paths: vec![]
            }
        );
    }

    #[test]
    fn a_relative_path_is_joined_to_the_cwd() {
        let cwd = root().join("work");
        let got = parse_words(&["notes", "a/b"], &cwd).unwrap();
        assert_eq!(got.paths, vec![cwd.join("notes"), cwd.join("a/b")]);
        assert!(got.paths.iter().all(|p| p.is_absolute()));
    }

    #[test]
    fn an_unknown_option_is_refused() {
        let cwd = root();
        assert_eq!(
            parse_words(&["--verbose"], &cwd),
            Err("unknown option --verbose".into())
        );
        assert_eq!(
            parse_words(&["notes", "-k"], &cwd),
            Err("unknown option -k".into())
        );
    }
}
