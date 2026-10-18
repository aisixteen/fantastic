//! FOLDS — a compaction MARK, not a deletion.
//!
//! A fold is an ordinary appended row that names the rows it collapses (`refs`),
//! carries a title (what the model reads every turn), a body (a short summary,
//! pulled if it wants more), and stays one descent from its members:
//! `grep`/`pull` on a fold id expand to its refs. Nothing is rewritten, nothing
//! is lost, and the history becomes a tree of marks the model walks down as far
//! as it needs.
//!
//! CHAINS ARE DERIVED, NEVER STORED. A chain is a root row plus everything
//! reachable from it through `parent`. A model attributes rows to interleaved
//! chains from `parent` alone, so the
//! agent carries no thread concept — this module walks the links when a
//! projection needs to know what is closed.
//!
//! A FOLD CARRIES ITS OWN PLACE. Its `anchor` is the smallest LEAF id it covers
//! (a leaf is a non-fold row; a member fold contributes its own anchor). Row ids
//! come from the kernel's one counter, so an anchor orders a fold in time without
//! any of its members present: the view array names folds without the rows they
//! hide, and `context_forget` prunes those rows from the store. A fold written
//! before folds carried the field is anchored by the `anchors` row
//! `context_forget` recorded for it, or, while its members are still stored, by
//! deriving the same value from its refs.
//!
//! Mirrors python `ai_core/folds.py`, which is the immutable reference.

use crate::tool_parse::extract_tool_calls;
use fantastic_bundle::py_json_dumps;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// The opener a fold row's rendered content carries.
pub const FOLD_TAG: &str = "[fold ";

pub use crate::constants::SUMMARY_INPUT_CAP;

/// A row's `id`, when it has one.
#[must_use]
pub fn row_id(row: &Value) -> Option<i64> {
    row.get("id").and_then(Value::as_i64)
}

/// A row's `parent`, when it has one.
#[must_use]
pub fn row_parent(row: &Value) -> Option<i64> {
    row.get("parent").and_then(Value::as_i64)
}

/// A row's `content` as text (the empty string when absent).
#[must_use]
pub fn row_content(row: &Value) -> &str {
    row.get("content").and_then(Value::as_str).unwrap_or("")
}

/// A row's `role`.
#[must_use]
pub fn row_role(row: &Value) -> &str {
    row.get("role").and_then(Value::as_str).unwrap_or("")
}

#[must_use]
/// Is this row a fold MARK (rather than a turn)?
pub fn is_fold(row: &Value) -> bool {
    row.get("kind").and_then(Value::as_str) == Some("fold")
}

/// The `refs` a fold names.
#[must_use]
pub fn refs_of(row: &Value) -> Vec<i64> {
    row.get("refs")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// Row indices grouped by root, in root arrival order. A row whose parent is
/// unknown (absent, or pointing outside `rows`) is its own root.
#[must_use]
pub fn chains(rows: &[Value]) -> Vec<Vec<usize>> {
    let by_id: HashMap<i64, usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| row_id(r).map(|id| (id, i)))
        .collect();
    let mut root_of: HashMap<usize, usize> = HashMap::new();
    for i in 0..rows.len() {
        if root_of.contains_key(&i) {
            continue;
        }
        let mut seen = vec![i];
        let mut cur = i;
        while let Some(&j) = row_parent(&rows[cur]).and_then(|par| by_id.get(&par)) {
            if seen.contains(&j) {
                break;
            }
            seen.push(j);
            cur = j;
        }
        for k in seen {
            root_of.insert(k, cur);
        }
    }
    // Groups in ROOT INDEX order (python sorts the group keys).
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..rows.len() {
        groups.entry(root_of[&i]).or_default().push(i);
    }
    let mut keys: Vec<usize> = groups.keys().copied().collect();
    keys.sort_unstable();
    keys.into_iter().map(|k| groups[&k].clone()).collect()
}

/// A chain is OPEN while its last row is an assistant turn that made a `send` —
/// a reply is still owed. A cancelled or failed call settles as a reply row, so
/// a cancel closes a chain the same way an answer does.
#[must_use]
pub fn is_open(rows: &[Value], chain: &[usize]) -> bool {
    let Some(&last_i) = chain.last() else {
        return false;
    };
    let last = &rows[last_i];
    if row_role(last) != "assistant" {
        return false;
    }
    extract_tool_calls(row_content(last))
        .iter()
        .any(|c| c.name == "send")
}

/// Every id named by any fold's refs.
#[must_use]
pub fn covered(rows: &[Value]) -> HashSet<i64> {
    let mut out = HashSet::new();
    for r in rows {
        if is_fold(r) {
            out.extend(refs_of(r));
        }
    }
    out
}

/// A fold title lives inside the tag; it may not close it or quote itself.
#[must_use]
pub fn clean_title(t: &str) -> String {
    let squashed: String = t
        .replace(']', ")")
        .replace('"', "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cut: String = squashed.chars().take(140).collect();
    if cut.is_empty() {
        "(untitled)".to_string()
    } else {
        cut
    }
}

/// A fold row. Its id is MINTED BY THE STORE and passed in — a fold is a message
/// like any other, never self-named. `anchor` is the smallest leaf id it covers
/// (`member_anchor` over its members).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn make_fold(
    refs: &[i64],
    title: &str,
    body: &str,
    degraded: bool,
    id: i64,
    anchor: i64,
    lvl: i64,
    parent: Option<i64>,
) -> Value {
    let title = clean_title(title);
    let head = format!("{FOLD_TAG}{id} · {} refs · L{lvl}] \"{title}\"", refs.len());
    let note = if degraded {
        "  (summary unavailable — refs and titles only)"
    } else {
        ""
    };
    json!({
        "role": "user",
        "id": id,
        // A fold sits where its members sat: its parent is its first member's
        // parent, so it stays inside the chain it collapsed.
        "parent": parent,
        "kind": "fold",
        "refs": refs,
        "title": title,
        "body": body,
        "level": lvl,
        "anchor": anchor,
        "content": format!("{head}{note}\n{body}"),
    })
}

/// An int as python's `_int` reads one: an integer, or a string of ASCII digits.
fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse().ok()
        }
        _ => None,
    }
}

/// A fold's own `anchor` field, when it carries one.
#[must_use]
pub fn own_anchor(fold: &Value) -> Option<i64> {
    fold.get("anchor").and_then(as_int)
}

/// The anchors `context_forget` recorded: every `anchors` row in `rows`, in
/// order, a later row winning per fold id.
#[must_use]
pub fn recorded_anchors(rows: &[Value]) -> HashMap<i64, i64> {
    let mut out = HashMap::new();
    for r in rows {
        if r.get("kind").and_then(Value::as_str) != Some("anchors") {
            continue;
        }
        let Some(m) = r.get("anchors").and_then(Value::as_object) else {
            continue;
        };
        for (k, v) in m {
            if let (Some(fid), Some(a)) = (as_int(&Value::String(k.clone())), as_int(v)) {
                out.insert(fid, a);
            }
        }
    }
    out
}

fn derive(
    fold: &Value,
    by_id: &HashMap<i64, &Value>,
    recorded: &HashMap<i64, i64>,
    seen: &HashSet<i64>,
) -> Option<i64> {
    let fid = row_id(fold);
    if fid.is_some_and(|f| seen.contains(&f)) {
        return None;
    }
    let mut seen = seen.clone();
    if let Some(f) = fid {
        seen.insert(f);
    }
    let mut best: Option<i64> = None;
    for reff in refs_of(fold) {
        let Some(r) = by_id.get(&reff) else {
            continue;
        };
        let a = if is_fold(r) {
            own_anchor(r)
                .or_else(|| recorded.get(&reff).copied())
                .or_else(|| derive(r, by_id, recorded, &seen))
        } else {
            Some(reff)
        };
        if let Some(a) = a {
            best = Some(best.map_or(a, |b| b.min(a)));
        }
    }
    best
}

/// Where `fold` sits: its own `anchor` field; else the anchor `context_forget`
/// recorded for it (`recorded`, from `recorded_anchors`); else the smallest leaf
/// id among its refs present in `rows`, through member folds recursively. None
/// when none of the three is known — such a fold is counted, never placed by
/// guessing.
#[must_use]
pub fn anchor_of(fold: &Value, rows: &[Value], recorded: &HashMap<i64, i64>) -> Option<i64> {
    if let Some(a) = own_anchor(fold) {
        return Some(a);
    }
    if let Some(a) = row_id(fold).and_then(|f| recorded.get(&f)) {
        return Some(*a);
    }
    let by_id: HashMap<i64, &Value> = rows
        .iter()
        .filter_map(|r| row_id(r).map(|id| (id, r)))
        .collect();
    derive(fold, &by_id, recorded, &HashSet::new())
}

/// fold id → anchor for every fold in `rows` whose anchor is known. `rows` are
/// the agent's HEAD rows (`anchors` rows included), never only the body being
/// rendered.
#[must_use]
pub fn anchors_of(rows: &[Value]) -> HashMap<i64, i64> {
    let recorded = recorded_anchors(rows);
    let by_id: HashMap<i64, &Value> = rows
        .iter()
        .filter_map(|r| row_id(r).map(|id| (id, r)))
        .collect();
    let mut out = HashMap::new();
    for r in rows {
        if !is_fold(r) {
            continue;
        }
        let Some(fid) = row_id(r) else {
            continue;
        };
        let a = own_anchor(r)
            .or_else(|| recorded.get(&fid).copied())
            .or_else(|| derive(r, &by_id, &recorded, &HashSet::new()));
        if let Some(a) = a {
            out.insert(fid, a);
        }
    }
    out
}

/// The anchor a new fold over `rows[i] for i in idxs` carries: the smallest leaf
/// id, a member fold contributing its anchor. A member fold with no known anchor
/// cannot be in the view, so it cannot be a member.
///
/// # Panics
/// A member fold without an anchor, a member row without an int id, or no
/// member: each is a programming error.
#[must_use]
pub fn member_anchor(rows: &[Value], idxs: &[usize], anchors: &HashMap<i64, i64>) -> i64 {
    let mut best: Option<i64> = None;
    for &i in idxs {
        let r = &rows[i];
        let a = if is_fold(r) {
            own_anchor(r)
                .or_else(|| row_id(r).and_then(|f| anchors.get(&f).copied()))
                .unwrap_or_else(|| panic!("fold: member fold {:?} has no anchor", row_id(r)))
        } else {
            row_id(r).unwrap_or_else(|| panic!("fold: member row {i} has no int id"))
        };
        best = Some(best.map_or(a, |b| b.min(a)));
    }
    best.expect("fold: a fold needs at least one member")
}

/// Row index → the folds placed before that row, each with its anchor.
type Placement<'a> = HashMap<usize, Vec<(i64, &'a Value)>>;

/// (placement, unanchored). Each fold not covered by another
/// fold in `rows` sits immediately before the first non-fold row whose id is at
/// or past its anchor, or at the end when there is none.
fn placed<'a>(rows: &'a [Value], anchors: &HashMap<i64, i64>) -> (Placement<'a>, usize) {
    let cov = covered(rows);
    // first_at(a) = the smallest row index whose id is >= a: ids sorted
    // ascending, each paired with the smallest index among the ids at or above.
    let mut eligible: Vec<(i64, usize)> = rows
        .iter()
        .enumerate()
        .filter(|(_, x)| !is_fold(x))
        .filter_map(|(i, x)| row_id(x).map(|id| (id, i)))
        .collect();
    eligible.sort_unstable();
    let mut least = vec![rows.len(); eligible.len() + 1];
    for j in (0..eligible.len()).rev() {
        least[j] = eligible[j].1.min(least[j + 1]);
    }
    let mut place: Placement<'a> = HashMap::new();
    let mut unanchored = 0;
    for r in rows {
        if !is_fold(r) || row_id(r).is_some_and(|id| cov.contains(&id)) {
            continue;
        }
        let a = own_anchor(r).or_else(|| row_id(r).and_then(|f| anchors.get(&f).copied()));
        let Some(a) = a else {
            unanchored += 1;
            continue;
        };
        let j = eligible.partition_point(|e| e.0 < a);
        place.entry(least[j]).or_default().push((a, r));
    }
    for v in place.values_mut() {
        v.sort_by_key(|p| p.0); // stable: same anchor keeps row order
    }
    (place, unanchored)
}

/// The view with folded rows hidden and each fold sitting at its anchor
/// (`anchors` from `anchors_of` over the HEAD — the members need not be in
/// `rows`). A fold covered by another fold in `rows` stays hidden; a fold whose
/// anchor is unknown is left out (`unanchored` counts it). Deterministic in
/// `rows`, so a view renders the same bytes until a new fold lands.
#[must_use]
pub fn render(rows: &[Value], anchors: &HashMap<i64, i64>) -> Vec<Value> {
    let cov = covered(rows);
    let (place, _) = placed(rows, anchors);
    let mut out: Vec<Value> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(fs) = place.get(&i) {
            out.extend(fs.iter().map(|(_, f)| (*f).clone()));
        }
        if row_id(r).is_some_and(|id| cov.contains(&id)) || is_fold(r) {
            continue; // hidden, or a fold rendered at its anchor
        }
        out.push(r.clone());
    }
    if let Some(fs) = place.get(&rows.len()) {
        out.extend(fs.iter().map(|(_, f)| (*f).clone()));
    }
    out
}

/// How many folds `render` leaves out because no anchor is known for them.
#[must_use]
pub fn unanchored(rows: &[Value], anchors: &HashMap<i64, i64>) -> usize {
    placed(rows, anchors).1
}

/// Replace fold ids with their refs, recursively — the descent path.
#[must_use]
pub fn expand_ids(rows: &[Value], ids: &[i64]) -> Vec<i64> {
    let folds: HashMap<i64, Vec<i64>> = rows
        .iter()
        .filter(|r| is_fold(r))
        .filter_map(|r| row_id(r).map(|id| (id, refs_of(r))))
        .collect();
    let mut out: Vec<i64> = Vec::new();
    let mut stack: Vec<i64> = ids.to_vec();
    let mut seen: HashSet<i64> = HashSet::new();
    while !stack.is_empty() {
        let x = stack.remove(0);
        if !seen.insert(x) {
            continue;
        }
        match folds.get(&x) {
            Some(refs) => {
                let mut next = refs.clone();
                next.append(&mut stack);
                stack = next;
            }
            None => out.push(x),
        }
    }
    out
}

/// 1 for a fold of rows, 2 for a fold of folds, and so on.
#[must_use]
pub fn level(rows: &[Value], fold_row: &Value) -> i64 {
    let by_id: HashMap<i64, &Value> = rows
        .iter()
        .filter_map(|r| row_id(r).map(|id| (id, r)))
        .collect();
    let mut best = 0;
    for reff in refs_of(fold_row) {
        if let Some(r) = by_id.get(&reff) {
            if is_fold(r) {
                best = best.max(level(rows, r));
            }
        }
    }
    best + 1
}

/// What the summarizer reads. CAPPED per row and in total: a chain with big
/// inline rows would otherwise make the fold call itself the reply that does
/// not fit.
#[must_use]
pub fn chain_text(rows: &[Value], chain: &[usize]) -> String {
    let per = (SUMMARY_INPUT_CAP / chain.len().max(1)).max(400);
    let items: Vec<Value> = chain
        .iter()
        .map(|&i| {
            let c = row_content(&rows[i]);
            let chars: Vec<char> = c.chars().collect();
            let text = if chars.len() > per {
                // python `c[: per // 2] + … + c[-per // 2 :]`: `-per // 2` FLOORS,
                // so the tail is `(per + 1) / 2` — one longer than the head when
                // `per` is odd. Halving both ends dropped a character.
                let head_n = per / 2;
                let tail_n = per.div_ceil(2);
                let head: String = chars[..head_n].iter().collect();
                let tail: String = chars[chars.len() - tail_n..].iter().collect();
                format!("{head} …[{} chars elided]… {tail}", chars.len() - per)
            } else {
                c.to_string()
            };
            json!({"role": row_role(&rows[i]), "id": row_id(&rows[i]), "content": text})
        })
        .collect();
    // python `json.dumps(items, ensure_ascii=False, default=str)[:CAP]` — the
    // separators are `, `/`: `, non-ASCII rides RAW, and the cut is code points.
    let s = py_json_dumps(&Value::Array(items), false, None);
    s.chars().take(SUMMARY_INPUT_CAP).collect()
}

/// The `· "<title>"` a rendered row carries, if any — python's
/// `re.search(r'· "([^"]{1,140})"', content)`. No regex crate: a hand scan.
#[must_use]
pub fn quoted_title(content: &str) -> Option<&str> {
    let bytes = content.as_bytes();
    let mut i = 0;
    while let Some(rel) = content[i..].find("· \"") {
        let start = i + rel + "· \"".len();
        if let Some(close) = content[start..].find('"') {
            // 1..=140 CODE POINTS, as python's `[^"]{1,140}` — a byte bound cut a
            // non-ASCII title short of what python matches.
            let n = content[start..start + close].chars().count();
            if (1..=140).contains(&n) {
                return Some(&content[start..start + close]);
            }
        }
        i = start.min(bytes.len());
        if i >= content.len() {
            break;
        }
    }
    None
}
