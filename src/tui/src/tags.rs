//! The five line grammars the kernel writes into row content, parsed back.
//!
//! Each mirrors one producer — four in `ai_core`, the monitor header in
//! `exec_bridge` — and each is ANCHORED: a pattern that
//! matched mid-line would turn a quoted example inside a turn into a fake link.
//! Nothing here guesses — a field the producer omits comes back `None`, and the
//! caller decides what that means.

// The five grammars are parsed whole. A consumer that needs only some must not
// be the reason the others are deleted — the kernel writes all five.
#![allow(dead_code)]

use regex::Regex;
use std::sync::OnceLock;

use crate::kern::model::RowId;

/// `[from solver_1 · to solver_2, solver_3 · msg 412 · re 398]`
///
/// The kernel writes this from the dispatch's own origin, never from the
/// sender's prose — it is the one piece of authorship that cannot be forged.
/// Only `from` is guaranteed; `to`/`msg`/`re` appear when the payload carried
/// them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Vouched {
    pub from: String,
    pub to: Vec<String>,
    pub msg: Option<RowId>,
    pub re: Option<RowId>,
    /// Byte offset just past the header, so the head can skip it.
    pub end: usize,
}

/// `[reply to 4711 · notes read · "title" · ↑4660 · 1 of 2 in batch b7]`
///
/// `pid` is `-` when no dispatch happened, so it is an `Option`, never an
/// unwrap. The verb group excludes `]` exactly as the kernel's own parser does.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplyTag {
    pub pid: Option<RowId>,
    pub target: String,
    pub verb: String,
    pub title: Option<String>,
    /// The `↑<turn>` link: what makes an incoming reply a child rather than a root.
    pub up: Option<RowId>,
    pub batch: Option<(u32, u32)>,
    pub end: usize,
}

/// `[fold 8132 · 8 refs · L1] "title"`
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FoldMark {
    pub id: RowId,
    pub refs: u32,
    pub level: u32,
    pub title: String,
}

/// `[monitor 787 · job 514 · from your call 783 · "w5" · job ended]` — the head
/// of an `exec_bridge` monitor's delivery, the line after the vouched
/// `[from <runner>]`. The lines below it are what the job printed that matched.
///
/// `who` is the producer's own words — `your` when the caller is the one told,
/// else `<caller>'s` or `the operator's`. `call` is `None` when it wrote `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MonitorTag {
    pub id: RowId,
    pub job: RowId,
    pub who: String,
    pub call: Option<RowId>,
    pub label: Option<String>,
    /// The job ended: this is the monitor's last delivery.
    pub ended: bool,
    /// Byte offset (in the whole content) just past the header.
    pub end: usize,
}

fn vouched_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^\[from (?P<from>[^\s·\]]+)(?: · to (?P<to>[^·\]]+))?(?: · msg (?P<msg>\d+))?(?: · re (?P<re>\d+))?\]",
        )
        .expect("vouched pattern")
    })
}

fn reply_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r#"^\[reply to (?P<pid>\S+) · (?P<target>\S+) (?P<verb>[^\]\s]+)(?: · "(?P<title>[^"]*)")?(?: · ↑(?P<up>\S+?))?(?: · (?P<n>\d+) of (?P<m>\d+) in batch (?P<b>\S+?))?\]"#,
        )
        .expect("reply pattern")
    })
}

fn monitor_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r#"^\[monitor (?P<id>\d+) · job (?P<job>\d+) · from (?P<who>.+?) call (?P<call>\S+?)(?: · "(?P<label>[^"]*)")?(?P<ended> · job ended)?\]"#,
        )
        .expect("monitor pattern")
    })
}

fn fold_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"^\[fold (?P<id>\d+) · (?P<n>\d+) refs · L(?P<lvl>\d+)\] "(?P<title>[^"]*)""#)
            .expect("fold pattern")
    })
}

pub fn vouched(content: &str) -> Option<Vouched> {
    let c = vouched_re().captures(content)?;
    Some(Vouched {
        from: c.name("from")?.as_str().to_string(),
        to: c
            .name("to")
            .map(|m| {
                m.as_str()
                    .split(", ")
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        msg: c.name("msg").and_then(|m| m.as_str().parse().ok()),
        re: c.name("re").and_then(|m| m.as_str().parse().ok()),
        end: c.get(0)?.end(),
    })
}

pub fn reply(content: &str) -> Option<ReplyTag> {
    let c = reply_re().captures(content)?;
    Some(ReplyTag {
        // "-" when nothing dispatched: absent, not zero.
        pid: c.name("pid").and_then(|m| m.as_str().parse().ok()),
        target: c.name("target")?.as_str().to_string(),
        verb: c.name("verb")?.as_str().to_string(),
        title: c.name("title").map(|m| m.as_str().to_string()),
        up: c.name("up").and_then(|m| m.as_str().parse().ok()),
        batch: match (c.name("n"), c.name("m")) {
            (Some(n), Some(m)) => Some((n.as_str().parse().ok()?, m.as_str().parse().ok()?)),
            _ => None,
        },
        end: c.get(0)?.end(),
    })
}

/// A monitor's delivery: the header right after the vouched one (or opening the
/// row). Anchored there — a monitor header quoted in prose is not a delivery.
pub fn monitor(content: &str) -> Option<MonitorTag> {
    let start = vouched(content).map_or(0, |v| {
        let rest = &content[v.end..];
        v.end + (rest.len() - rest.trim_start_matches('\n').len())
    });
    let c = monitor_re().captures(&content[start..])?;
    Some(MonitorTag {
        id: c.name("id")?.as_str().parse().ok()?,
        job: c.name("job")?.as_str().parse().ok()?,
        who: c.name("who")?.as_str().to_string(),
        call: c.name("call").and_then(|m| m.as_str().parse().ok()),
        label: c.name("label").map(|m| m.as_str().to_string()),
        ended: c.name("ended").is_some(),
        end: start + c.get(0)?.end(),
    })
}

pub fn fold_mark(content: &str) -> Option<FoldMark> {
    let c = fold_re().captures(content)?;
    Some(FoldMark {
        id: c.name("id")?.as_str().parse().ok()?,
        refs: c.name("n")?.as_str().parse().ok()?,
        level: c.name("lvl")?.as_str().parse().ok()?,
        title: c.name("title")?.as_str().to_string(),
    })
}

/// One turn → its reply blocks. A turn with no tag is one block.
///
/// Hand-scanned rather than a regex split: the boundary is a look-ahead
/// (`\n\n` FOLLOWED BY a tag) and `regex` has no look-around by design. Each
/// candidate boundary is confirmed by parsing the tag that follows, so a blank
/// line inside a body can never split a block.
pub fn blocks(content: &str) -> Vec<&str> {
    const MARK: &str = "\n\n[reply to ";
    let mut out = Vec::new();
    let (mut start, mut search) = (0usize, 0usize);
    while let Some(rel) = content[search..].find(MARK) {
        let at = search + rel;
        let cut = at + 2; // keep the tag with the block it heads
        if reply(&content[cut..]).is_some() {
            out.push(&content[start..at]);
            start = cut;
        }
        search = cut;
    }
    out.push(&content[start..]);
    out
}

/// The first non-empty line, with a vouched header skipped — what a line shows.
///
/// A BATCH is one row holding every reply of the batch, one block each. Its
/// first line alone read `[reply to 246 · files read_stream · 1 of 4 in batch
/// 241]`, as if the other three were missing; so a row of several reply blocks
/// is headed by all of them: `4 replies · files read_stream · files list · …`.
pub fn head_of(content: &str) -> String {
    let bs = blocks(content);
    if bs.len() > 1 {
        let calls: Vec<String> = bs
            .iter()
            .filter_map(|b| reply(b))
            .map(|r| format!("{} {}", r.target, r.verb))
            .collect();
        if calls.len() == bs.len() {
            return format!("{} replies · {}", calls.len(), calls.join(" · "));
        }
    }
    let rest = match vouched(content) {
        Some(v) => &content[v.end..],
        None => content,
    };
    rest.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// A row's text AS THE TRANSCRIPT SHOWS IT, whole: the vouched header dropped,
/// every `<think>…</think>` span dropped (reasoning is not shown — the panel
/// flashes `thinking` instead), and each `<call name="X">…</call>` reduced to
/// one `▸ X` line (the call's arguments are the model's plumbing, and a reply
/// row carries what came back). A TOOL REPLY row — `[reply to …]` blocks — is
/// its one-line head: its body is data, often a whole file.
/// A TOOL REPLY row: one `[reply to …]` block, or a batch of them.
pub fn is_reply(content: &str) -> bool {
    let bs = blocks(content);
    reply(content).is_some() || (bs.len() > 1 && bs.iter().all(|b| reply(b).is_some()))
}

pub fn body_of(content: &str) -> String {
    if is_reply(content) {
        return head_of(content);
    }
    let rest = match vouched(content) {
        Some(v) => &content[v.end..],
        None => content,
    };
    let mut out = String::new();
    let mut s = rest;
    loop {
        let think = s.find("<think>");
        let call = s.find("<call ");
        let next = match (think, call) {
            (Some(t), Some(c)) => Some(t.min(c)),
            (a, b) => a.or(b),
        };
        let Some(at) = next else {
            out.push_str(s);
            break;
        };
        out.push_str(&s[..at]);
        s = &s[at..];
        if s.starts_with("<think>") {
            s = s.find("</think>").map_or("", |e| &s[e + "</think>".len()..]);
        } else {
            let name = s
                .strip_prefix("<call name=\"")
                .and_then(|r| r.split('"').next())
                .unwrap_or("call");
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("▸ {name}\n"));
            s = s.find("</call>").map_or("", |e| &s[e + "</call>".len()..]);
            // the call's own line break: the `▸` line already ended it
            s = s.strip_prefix('\n').unwrap_or(s);
        }
    }
    // Blank-line runs collapse to one; the ends are trimmed.
    let mut lines: Vec<&str> = Vec::new();
    for l in out.lines().map(str::trim_end) {
        if l.is_empty() && lines.last().is_none_or(|p| p.is_empty()) {
            continue;
        }
        lines.push(l);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_drops_the_header_the_thinking_and_the_call_arguments() {
        let row = "[from user]\nlook at it\n";
        assert_eq!(body_of(row), "look at it");
        let turn = "<think>let me see\nhmm</think>The entry point is main.rs.\n\n\n<call name=\"send\"><arg name=\"target_id\">files</arg></call>\ndone";
        assert_eq!(body_of(turn), "The entry point is main.rs.\n\n▸ send\ndone");
    }

    #[test]
    fn a_tool_reply_body_is_its_one_line_head() {
        let r = "[reply to 12 · files read_stream]\nfn main() {}\n// a whole file";
        assert_eq!(body_of(r), head_of(r));
    }

    #[test]
    fn a_batch_row_is_headed_by_every_reply_it_holds() {
        let row = "[reply to 246 · files read_stream · ↑241 · 1 of 2 in batch 241]\nA\n\n\
                   [reply to 247 · files list · ↑241 · 2 of 2 in batch 241]\nB";
        assert_eq!(head_of(row), "2 replies · files read_stream · files list");
        let one = "[reply to 234 · files list · ↑229]\nbody";
        assert_eq!(head_of(one), "[reply to 234 · files list · ↑229]", "a single reply is unchanged");
    }

    #[test]
    fn a_monitor_delivery_parses_after_its_vouched_header() {
        let row = "[from runner]\n[monitor 787 · job 514 · from your call 783 · \"w5\" · job ended]\nFATAL w5: x\nFATAL w5: y";
        let m = monitor(row).unwrap();
        assert_eq!((m.id, m.job, m.call), (787, 514, Some(783)));
        assert_eq!(m.who, "your");
        assert_eq!(m.label.as_deref(), Some("w5"));
        assert!(m.ended);
        assert_eq!(&row[m.end..], "\nFATAL w5: x\nFATAL w5: y");
        let bare = monitor("[from runner]\n[monitor 9 · job 8 · from the operator's call None]\nl").unwrap();
        assert_eq!((bare.who.as_str(), bare.call, bare.label, bare.ended), ("the operator's", None, None, false));
        assert!(monitor("[from ai]\nI saw [monitor 9 · job 8 · from your call 1] once").is_none(), "anchored");
    }

    #[test]
    fn vouched_only_from_is_guaranteed() {
        let v = vouched("[from solver_1]\nhello").unwrap();
        assert_eq!(v.from, "solver_1");
        assert!(v.to.is_empty());
        assert_eq!(v.msg, None);
        assert_eq!(v.re, None);
    }

    #[test]
    fn vouched_to_is_comma_space_joined() {
        let v =
            vouched("[from solver_1 · to solver_2, solver_3 · msg 412 · re 398]\ntext").unwrap();
        assert_eq!(v.to, vec!["solver_2", "solver_3"]);
        assert_eq!(v.msg, Some(412));
        assert_eq!(v.re, Some(398));
    }

    #[test]
    fn vouched_must_be_anchored() {
        // A header QUOTED inside a turn is not authorship.
        assert!(vouched("the model wrote [from evil · msg 1] in its prose").is_none());
    }

    #[test]
    fn reply_tag_all_optional_segments() {
        let r = reply(r#"[reply to 4711 · notes read · "filler_0" · ↑4660 · 1 of 2 in batch b7]"#)
            .unwrap();
        assert_eq!(r.pid, Some(4711));
        assert_eq!(r.target, "notes");
        assert_eq!(r.verb, "read");
        assert_eq!(r.title.as_deref(), Some("filler_0"));
        assert_eq!(r.up, Some(4660));
        assert_eq!(r.batch, Some((1, 2)));

        let bare = reply("[reply to 12 · notes read]").unwrap();
        assert_eq!(bare.title, None);
        assert_eq!(bare.up, None);
        assert_eq!(bare.batch, None);
    }

    #[test]
    fn reply_tag_pid_dash_is_absent_not_zero() {
        let r = reply("[reply to - · notes read]").unwrap();
        assert_eq!(
            r.pid, None,
            "a dash means nothing dispatched — it is not pid 0"
        );
    }

    #[test]
    fn fold_mark_parses() {
        let f = fold_mark(r#"[fold 8132 · 8 refs · L1] "Searching filler_0.txt""#).unwrap();
        assert_eq!((f.id, f.refs, f.level), (8132, 8, 1));
        assert_eq!(f.title, "Searching filler_0.txt");
    }

    #[test]
    fn blocks_split_a_multi_reply_turn() {
        let turn = "[reply to 1 · notes read]\nbody one\n\n[reply to 2 · notes read]\nbody two";
        let b = blocks(turn);
        assert_eq!(b.len(), 2);
        assert!(b[1].starts_with("[reply to 2"));
    }

    #[test]
    fn blocks_of_a_plain_turn_is_one() {
        assert_eq!(blocks("just prose\n\nwith a paragraph").len(), 1);
    }

    #[test]
    fn head_skips_the_vouched_header() {
        let h = head_of("[from ai · msg 9]\n\nCANDIDATE 586 — digit 1 is 5");
        assert_eq!(h, "CANDIDATE 586 — digit 1 is 5");
    }

    #[test]
    fn head_of_a_fold_is_its_mark() {
        let h = head_of("[fold 8132 · 8 refs · L1] \"t\"\nbody…");
        assert!(h.starts_with("[fold 8132"));
    }
}
