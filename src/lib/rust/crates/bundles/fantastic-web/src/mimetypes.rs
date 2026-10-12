//! python `mimetypes.guess_type` — the file proxy's `content-type`.
//!
//! `web/app.py:135` types the octet proxy with `mimetypes.guess_type(path)` and
//! hands the result to starlette, which appends `; charset=utf-8` to any
//! `text/*` media type (`starlette/responses.py:76-79`) and falls back to
//! `application/octet-stream` when the table has no answer. The table itself is
//! CPython's `_types_map_default`, generated verbatim into
//! [`mimetypes_table`](super::mimetypes_table).
//!
//! RESIDUAL: CPython's `mimetypes.init()` also reads the host's
//! `knownfiles` (on this machine `/etc/apache2/mime.types` exists), which
//! OVERRIDES the built-in table for a handful of extensions — `.js` types as
//! `text/javascript` there and `application/javascript` here. A host `/etc`
//! file is not portable source, so the built-in table is what this port
//! reproduces.

use crate::mimetypes_table::{ENCODINGS_MAP, SUFFIX_MAP, TYPES_MAP};

/// `posixpath.splitext` — split at the last dot of the basename, leading dots
/// excluded (`".bashrc"` has no extension).
fn splitext(p: &str) -> (&str, &str) {
    let base_start = p.rfind('/').map_or(0, |i| i + 1);
    let name = &p[base_start..];
    // Skip leading dots: python's splitext never treats them as a separator.
    let lead = name.len() - name.trim_start_matches('.').len();
    match name[lead..].rfind('.') {
        Some(i) => {
            let cut = base_start + lead + i;
            (&p[..cut], &p[cut..])
        }
        None => (p, ""),
    }
}

/// `mimetypes.guess_type(path)[0]` — `None` when the table has no answer.
#[must_use]
pub fn guess_type(path: &str) -> Option<&'static str> {
    let (b, e) = splitext(path);
    let (mut base, mut ext) = (b.to_string(), e.to_string());
    // Compound suffixes (`.tgz` → `.tar.gz`) rewrite and re-split, repeatedly.
    loop {
        let lower = ext.to_lowercase();
        let Some((_, repl)) = SUFFIX_MAP.iter().find(|(k, _)| *k == lower) else {
            break;
        };
        let joined = format!("{base}{repl}");
        let (b, e) = splitext(&joined);
        base = b.to_string();
        ext = e.to_string();
    }
    // An ENCODING suffix names the compression, not the type: strip and re-split.
    if ENCODINGS_MAP.contains(&ext.as_str()) {
        let (_, e) = splitext(&base);
        ext = e.to_string();
    }
    lookup(&ext)
}

fn lookup(ext: &str) -> Option<&'static str> {
    let lower = ext.to_lowercase();
    TYPES_MAP.iter().find(|(k, _)| *k == lower).map(|(_, v)| *v)
}

/// The `content-type` header value the file proxy sends: the guessed type, or
/// `application/octet-stream`, with starlette's `; charset=utf-8` on `text/*`.
#[must_use]
pub fn content_type(path: &str) -> String {
    let mime = guess_type(path).unwrap_or("application/octet-stream");
    if mime.starts_with("text/") && !mime.contains("charset=") {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    }
}
