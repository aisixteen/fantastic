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
/// like any other, never self-named.
#[must_use]
pub fn make_fold(
    refs: &[i64],
    title: &str,
    body: &str,
    degraded: bool,
    id: i64,
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
        "content": format!("{head}{note}\n{body}"),
    })
}

/// The view with folded rows hidden and each fold sitting where its first ref
/// was. Deterministic in `rows`, so a view renders the same bytes until a new
/// fold lands.
#[must_use]
pub fn render(rows: &[Value]) -> Vec<Value> {
    let cov = covered(rows);
    let mut first_pos: HashMap<i64, usize> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(rid) = row_id(r) {
            if cov.contains(&rid) {
                first_pos.entry(rid).or_insert(i);
            }
        }
    }
    let mut place: HashMap<usize, Vec<Value>> = HashMap::new();
    for r in rows {
        if !is_fold(r) {
            continue;
        }
        let at = refs_of(r)
            .iter()
            .filter_map(|x| first_pos.get(x))
            .min()
            .copied();
        if let Some(at) = at {
            if row_id(r).is_none_or(|id| !cov.contains(&id)) {
                place.entry(at).or_default().push(r.clone());
            }
        }
    }
    let mut out: Vec<Value> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(fs) = place.get(&i) {
            out.extend(fs.iter().cloned());
        }
        if row_id(r).is_some_and(|id| cov.contains(&id)) || is_fold(r) {
            continue; // hidden, or a fold rendered at its first ref
        }
        out.push(r.clone());
    }
    out
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
