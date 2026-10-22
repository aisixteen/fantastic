//! The Windows side of [`crate::sys`]: no login shell, and programs by their
//! `PATHEXT` extension (python `shutil.which`).

use std::path::Path;

/// python `shutil._WIN_DEFAULT_PATHEXT`: what `which` tries when `PATHEXT` is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.JS;.WS;.MSC";

/// Windows has no login shell: a process's PATH is the user's own (the
/// registry's, as Explorer hands it to every program), so there is no second
/// PATH to merge.
pub(crate) async fn login_path() -> Option<String> {
    None
}

/// The executable extensions, in `which`'s order: `PATHEXT` from this process's
/// environment, as `which` reads `os.environ`.
fn pathext() -> Vec<String> {
    let source = std::env::var("PATHEXT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PATHEXT.to_string());
    source
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect()
}

/// The name a shell runs the file named `file` by, and its rank among the files
/// of one directory that share that name (lower wins) — python `shutil.which`: a
/// file ending in a `PATHEXT` extension is run by its name WITHOUT it (`git.exe`
/// is `git`), and `which git` tries the extensions in `PATHEXT` order (`.COM`
/// before `.EXE`). Any other file is no program.
pub(crate) fn program_name(file: &str) -> Option<(String, usize)> {
    pathext().iter().enumerate().find_map(|(rank, ext)| {
        // the extensions are ASCII, matched case-insensitively, as the OS does
        let cut = file
            .len()
            .checked_sub(ext.len())
            .filter(|&c| c > 0 && file.is_char_boundary(c))?;
        file[cut..]
            .eq_ignore_ascii_case(ext)
            .then(|| (file[..cut].to_string(), rank))
    })
}

/// Whether the file at `p` (links followed) is a program: any regular file —
/// Windows has no execute bit (its extension is what makes it one,
/// [`program_name`]).
pub(crate) fn is_program(p: &Path) -> bool {
    p.is_file()
}

/// TEST ONLY: a file in `dir`, listed as `stem`, that is a program
/// (`<stem>.exe`) or not (`<stem>`, no extension). Returns its path.
#[cfg(test)]
pub(crate) fn program_fixture(dir: &Path, stem: &str, runs: bool) -> std::path::PathBuf {
    let f = dir.join(if runs {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    });
    std::fs::write(&f, "x").unwrap();
    f
}
