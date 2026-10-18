//! THE REGISTRY AND THE RUN FILE, parsed from text.
//!
//! ```text
//!   <home>/fantastics/<folder>/fantastic.yaml   one Registered entry
//!   <folder>/.fantastic/cli/canvas.yaml         one CanvasRun, while a canvas serves
//! ```
//!
//! A REGISTRY ENTRY is written by the CLI's `user_store` agent:
//! `{guid, name, path, pid, canvas, stored_at, last_seen}`. Without a guid or a
//! path it is not an entry (`None`); a pid or a canvas that is not what it must
//! be is absent (a stale or missing pid is not an error).
//!
//! A CANVAS is a page on THIS machine only: `http`, a loopback host, an explicit
//! port. Anything else is not a canvas the home may open or ask.

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

/// The largest pid any of the supported systems hands out (`i32::MAX`).
const PID_MAX: i64 = 2_147_483_647;

/// The hosts a local canvas may name, as they are written in its URL.
const LOCAL_HOSTS: [&str; 3] = ["127.0.0.1", "[::1]", "localhost"];

/// A canvas page served on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Canvas {
    pub url: String,
    pub port: u16,
}

impl Canvas {
    /// The canvas at `url`, if it is one on this machine: scheme `http`, host
    /// exactly `127.0.0.1`, `[::1]` or `localhost`, and an explicit port in
    /// `1..=65535`. Anything else is `None`.
    pub fn local(url: &str) -> Option<Canvas> {
        let rest = url.strip_prefix("http://")?;
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..end];
        let (host, port) = if authority.starts_with('[') {
            let close = authority.find(']')?;
            let (host, after) = authority.split_at(close + 1);
            (host, after.strip_prefix(':')?)
        } else {
            authority.rsplit_once(':')?
        };
        if !LOCAL_HOSTS.contains(&host) {
            return None;
        }
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let port: u16 = port.parse().ok()?;
        if port == 0 {
            return None;
        }
        Some(Canvas {
            url: url.to_string(),
            port,
        })
    }
}

/// One registry entry: one fantastic the home knows of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    /// The registry folder the entry was read from (`<home>/fantastics/<folder>`).
    pub folder: String,
    pub guid: String,
    pub name: Option<String>,
    pub path: String,
    pub pid: Option<u32>,
    pub canvas: Option<Canvas>,
    pub stored_at: i64,
    pub last_seen: i64,
}

impl Registered {
    /// Parse the entry in `folder` from its `fantastic.yaml` text. Not a mapping,
    /// no guid or no path is `None`; see the module docs for each field.
    pub fn parse(folder: &str, yaml: &str) -> Option<Registered> {
        let doc: Value = serde_yaml::from_str(yaml).ok()?;
        let map = doc.as_mapping()?;
        let guid = map
            .get("guid")
            .and_then(scalar_text)
            .filter(|g| !g.is_empty())?;
        let path = map
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())?
            .to_string();
        let name = map
            .get("name")
            .and_then(scalar_text)
            .filter(|n| !n.is_empty());
        let pid = map.get("pid").and_then(pid_of);
        let canvas = map
            .get("canvas")
            .and_then(Value::as_str)
            .and_then(Canvas::local);
        let stored_at = map.get("stored_at").and_then(Value::as_i64).unwrap_or(0);
        let last_seen = map.get("last_seen").and_then(Value::as_i64).unwrap_or(0);
        Some(Registered {
            folder: folder.to_string(),
            guid,
            name,
            path,
            pid,
            canvas,
            stored_at,
            last_seen,
        })
    }

    /// Its name, else the last component of its path.
    pub fn display_name(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => last_component(&self.path).to_string(),
        }
    }

    /// One run of one fantastic as the registry describes it:
    /// `"{guid} {pid or 0} {canvas url or empty}"`.
    pub fn run_key(&self) -> String {
        let url = self.canvas.as_ref().map_or("", |c| c.url.as_str());
        format!("{} {} {}", self.guid, self.pid.unwrap_or(0), url)
    }
}

/// The registry from its `(folder, yaml text)` entries: what does not parse is
/// skipped, the rest ordered by `(stored_at, folder)`, and of several entries
/// with one guid only the first is kept.
pub fn registered(entries: Vec<(String, String)>) -> Vec<Registered> {
    let mut parsed: Vec<Registered> = entries
        .iter()
        .filter_map(|(folder, yaml)| Registered::parse(folder, yaml))
        .collect();
    parsed.sort_by(|a, b| (a.stored_at, &a.folder).cmp(&(b.stored_at, &b.folder)));
    let mut kept: Vec<Registered> = Vec::with_capacity(parsed.len());
    for reg in parsed {
        if !kept.iter().any(|k| k.guid == reg.guid) {
            kept.push(reg);
        }
    }
    kept
}

/// A folder's run file `.fantastic/cli/canvas.yaml` `{pid, port, url}`, written
/// while `fantastic canvas` serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasRun {
    pub pid: u32,
    pub port: u16,
    pub canvas: Canvas,
}

impl CanvasRun {
    /// Parse a run file: a mapping, a pid in `1..=2147483647`, a port in
    /// `1..=65535`, and a local `url` on that same port. Anything else is `None`.
    pub fn parse(yaml: &str) -> Option<CanvasRun> {
        let doc: Value = serde_yaml::from_str(yaml).ok()?;
        let map = doc.as_mapping()?;
        let pid = map.get("pid").and_then(pid_of)?;
        let port = map.get("port").and_then(Value::as_i64)?;
        let port = u16::try_from(port).ok().filter(|p| *p != 0)?;
        let canvas = map
            .get("url")
            .and_then(Value::as_str)
            .and_then(Canvas::local)?;
        if canvas.port != port {
            return None;
        }
        Some(CanvasRun { pid, port, canvas })
    }
}

/// The last component of a path, split on `/` and `\`, a trailing separator ignored.
pub(crate) fn last_component(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}

/// A scalar (string, number, bool) as text; null and collections are not text.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// An integer pid in `1..=2147483647`.
fn pid_of(value: &Value) -> Option<u32> {
    let pid = value.as_i64().filter(|p| (1..=PID_MAX).contains(p))?;
    u32::try_from(pid).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(yaml: &str) -> Option<Registered> {
        Registered::parse("f1", yaml)
    }

    const FULL: &str = "guid: g-1\nname: Notes\npath: /home/a/notes\npid: 4242\ncanvas: http://127.0.0.1:8123/\nstored_at: 17\nlast_seen: 99\n";

    #[test]
    fn a_full_entry_parses() {
        let reg = entry(FULL).expect("a full entry parses");
        assert_eq!(reg.folder, "f1");
        assert_eq!(reg.guid, "g-1");
        assert_eq!(reg.name.as_deref(), Some("Notes"));
        assert_eq!(reg.path, "/home/a/notes");
        assert_eq!(reg.pid, Some(4242));
        assert_eq!(
            reg.canvas,
            Some(Canvas {
                url: "http://127.0.0.1:8123/".into(),
                port: 8123
            })
        );
        assert_eq!((reg.stored_at, reg.last_seen), (17, 99));
        assert_eq!(reg.display_name(), "Notes");
        assert_eq!(reg.run_key(), "g-1 4242 http://127.0.0.1:8123/");
    }

    #[test]
    fn a_minimal_entry_has_defaults() {
        let reg = entry("guid: g\npath: /p/q/\n").expect("guid and path are enough");
        assert_eq!(
            (reg.name.clone(), reg.pid, reg.canvas.clone()),
            (None, None, None)
        );
        assert_eq!((reg.stored_at, reg.last_seen), (0, 0));
        assert_eq!(reg.display_name(), "q");
        assert_eq!(reg.run_key(), "g 0 ");
    }

    #[test]
    fn not_a_mapping_is_not_an_entry() {
        assert_eq!(entry("- guid: g\n- path: /p\n"), None);
        assert_eq!(entry("just text"), None);
        assert_eq!(entry(""), None);
        assert_eq!(entry("guid: [unclosed"), None);
    }

    #[test]
    fn guid_and_path_are_required() {
        assert_eq!(entry("path: /p\n"), None);
        assert_eq!(entry("guid: ''\npath: /p\n"), None);
        assert_eq!(entry("guid: ~\npath: /p\n"), None);
        assert_eq!(entry("guid: [a]\npath: /p\n"), None);
        assert_eq!(entry("guid: g\n"), None);
        assert_eq!(entry("guid: g\npath: ''\n"), None);
        assert_eq!(entry("guid: g\npath: 12\n"), None);
    }

    #[test]
    fn a_scalar_guid_is_text() {
        assert_eq!(
            entry("guid: 12345\npath: /p\n").map(|r| r.guid),
            Some("12345".into())
        );
        assert_eq!(
            entry("guid: true\npath: /p\n").map(|r| r.guid),
            Some("true".into())
        );
        assert_eq!(
            entry("guid: 1.5\npath: /p\n").map(|r| r.guid),
            Some("1.5".into())
        );
    }

    #[test]
    fn a_null_or_empty_name_falls_to_the_path() {
        let null = entry("guid: g\npath: C:\\work\\atlas\nname: ~\n").expect("parses");
        assert_eq!(null.name, None);
        assert_eq!(null.display_name(), "atlas");
        let empty = entry("guid: g\npath: /w/atlas\nname: ''\n").expect("parses");
        assert_eq!(empty.name, None);
        let number = entry("guid: g\npath: /w/atlas\nname: 7\n").expect("parses");
        assert_eq!(number.display_name(), "7");
    }

    #[test]
    fn a_pid_out_of_range_is_absent() {
        for pid in ["0", "-3", "2147483648", "4.5", "'12'"] {
            let reg = entry(&format!("guid: g\npath: /p\npid: {pid}\n")).expect("parses");
            assert_eq!(reg.pid, None, "pid {pid}");
        }
        // A number past YAML's integers makes the document itself unreadable.
        assert_eq!(
            entry("guid: g\npath: /p\npid: 99999999999999999999\n"),
            None
        );
        let top = entry("guid: g\npath: /p\npid: 2147483647\n").expect("parses");
        assert_eq!(top.pid, Some(2_147_483_647));
    }

    #[test]
    fn a_canvas_must_be_local() {
        for url in [
            "http://example.com:80",
            "https://127.0.0.1:8123",
            "http://127.0.0.1",
            "http://127.0.0.1:",
            "http://127.0.0.1:0",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:+80",
            "http://user@127.0.0.1:80",
            "http://127.0.0.2:80",
            "http://[::2]:80",
            "http://::1:80",
            "ftp://localhost:21",
        ] {
            assert_eq!(Canvas::local(url), None, "{url}");
            let reg = entry(&format!("guid: g\npath: /p\ncanvas: '{url}'\n")).expect("parses");
            assert_eq!(reg.canvas, None, "{url}");
        }
        assert_eq!(
            Canvas::local("http://[::1]:9000/x").map(|c| c.port),
            Some(9000)
        );
        assert_eq!(Canvas::local("http://localhost:1").map(|c| c.port), Some(1));
        assert_eq!(
            Canvas::local("http://127.0.0.1:65535?q").map(|c| c.port),
            Some(65535)
        );
        let v6 = entry("guid: g\npath: /p\ncanvas: 'http://[::1]:8000/'\n").expect("parses");
        assert_eq!(v6.canvas.map(|c| c.url), Some("http://[::1]:8000/".into()));
    }

    #[test]
    fn registered_orders_and_keeps_the_first_of_a_guid() {
        let e = |folder: &str, yaml: &str| (folder.to_string(), yaml.to_string());
        let list = registered(vec![
            e("c", "guid: g2\npath: /two\nstored_at: 5\n"),
            e("b", "guid: g1\npath: /one-late\nstored_at: 9\n"),
            e("x", "not: an entry\n"),
            e("a", "guid: g1\npath: /one\nstored_at: 5\n"),
            e("d", "guid: g3\npath: /three\n"),
        ]);
        let got: Vec<(&str, &str)> = list
            .iter()
            .map(|r| (r.folder.as_str(), r.path.as_str()))
            .collect();
        assert_eq!(got, vec![("d", "/three"), ("a", "/one"), ("c", "/two")]);
    }

    #[test]
    fn a_run_file_parses() {
        let run =
            CanvasRun::parse("pid: 77\nport: 8123\nurl: http://localhost:8123/\n").expect("parses");
        assert_eq!(run.pid, 77);
        assert_eq!(run.port, 8123);
        assert_eq!(run.canvas.url, "http://localhost:8123/");
    }

    #[test]
    fn a_bad_run_file_is_none() {
        for yaml in [
            "pid: 77\nport: 8124\nurl: http://localhost:8123/\n",
            "pid: 77\nport: 80\nurl: http://example.com:80/\n",
            "pid: 0\nport: 8123\nurl: http://localhost:8123/\n",
            "pid: -1\nport: 8123\nurl: http://localhost:8123/\n",
            "pid: 2147483648\nport: 8123\nurl: http://localhost:8123/\n",
            "port: 8123\nurl: http://localhost:8123/\n",
            "pid: 77\nport: 0\nurl: http://localhost:0/\n",
            "pid: 77\nport: 70000\nurl: http://localhost:70000/\n",
            "pid: 77\nport: '8123'\nurl: http://localhost:8123/\n",
            "pid: 77\nport: 8123\n",
            "- pid: 77\n",
            "",
        ] {
            assert_eq!(CanvasRun::parse(yaml), None, "{yaml}");
        }
    }
}
