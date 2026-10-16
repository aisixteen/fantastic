//! Windows: python's `ntpath` grammar.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// python `ntpath.expanduser` of a path starting `~`: the home is
/// `%USERPROFILE%`, else `%HOMEDRIVE%` joined with `%HOMEPATH%`; `~user` other
/// than `%USERNAME%` is the sibling of that home named `user` — guessed only
/// when the home's own name is `%USERNAME%`. `None` when nothing expands.
pub(crate) fn expanduser(p: &str) -> Option<String> {
    let i = p[1..].find(['/', '\\']).map_or(p.len(), |j| j + 1);
    let mut home = match std::env::var_os("USERPROFILE") {
        Some(h) => h.to_string_lossy().into_owned(),
        None => {
            let path = std::env::var_os("HOMEPATH")?;
            let drive = std::env::var_os("HOMEDRIVE").unwrap_or_default();
            PathBuf::from(drive)
                .join(path)
                .to_string_lossy()
                .into_owned()
        }
    };
    if i != 1 {
        let user = &p[1..i];
        let current = std::env::var("USERNAME").ok();
        if current.as_deref() != Some(user) {
            let (head, tail) = split(&home);
            if current.as_deref() != Some(tail) {
                return None;
            }
            home = Path::new(head).join(user).to_string_lossy().into_owned();
        }
    }
    Some(format!("{home}{}", &p[i..]))
}

/// `ntpath.split`: (head, tail) around the last separator, the head's trailing
/// separators dropped — never into its drive and root.
fn split(p: &str) -> (&str, &str) {
    let anchor = anchor_len(p);
    let i = anchor + p[anchor..].rfind(['/', '\\']).map_or(0, |j| j + 1);
    let head = p[anchor..i].trim_end_matches(['/', '\\']);
    (&p[..anchor + head.len()], &p[i..])
}

/// The byte length of `p`'s drive (or UNC share) and root.
fn anchor_len(p: &str) -> usize {
    let mut len = 0;
    for c in Path::new(p).components() {
        match c {
            Component::Prefix(pre) => len += pre.as_os_str().len(),
            Component::RootDir => len += 1,
            _ => break,
        }
    }
    len
}

/// pathlib's split of a `WindowsPath` into (parent, name): the drive and root
/// kept, `/` and `\` both separators, empty and `.` parts dropped, `..` kept,
/// the parent of a bare name `.`; the parent written with `\`, as python's
/// `str(path.parent)`.
pub(crate) fn split_path(p: &str) -> (String, String) {
    let mut anchor = PathBuf::new();
    let mut parts: Vec<&OsStr> = Vec::new();
    for c in Path::new(p).components() {
        match c {
            Component::Prefix(_) | Component::RootDir => anchor.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir | Component::Normal(_) => parts.push(c.as_os_str()),
        }
    }
    let name = parts
        .pop()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut parent = anchor;
    parent.extend(parts);
    let parent = if parent.as_os_str().is_empty() {
        ".".to_string()
    } else {
        parent.to_string_lossy().into_owned()
    };
    (parent, name)
}
