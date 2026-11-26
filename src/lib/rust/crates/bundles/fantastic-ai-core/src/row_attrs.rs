//! What a row keeps about itself (#618, #619), in its `attributes` map: `ts` and
//! `source` on every row; on an assistant row the request's `parts` as a DELTA against
//! the chat's previous request (a full list on a checkpoint), the running `totals`, and
//! the `cost` of the pass.
//!
//! THE MODEL SEES NONE OF THEM on a row: every attribute is stored and returned to
//! people and tools, and a request carries a row's content only
//! ([`crate::model_format::map`]). The date is said once, in the system block's
//! environment section, from the first row of the thread ([`day_of`]).

use fantastic_kernel::AgentId;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// Every this many requests the parts are a full list again (a checkpoint), so a
/// reader never replays more than this many deltas.
pub(crate) const CHECKPOINT_EVERY: u64 = 50;

/// The key of a row's UTC offset (#766, 1.13): whole seconds east of UTC, in force when
/// the row was written. Set once, where the row's `ts` is set ([`offset_at`]); the
/// environment section's date is read with it, never with the process's time zone.
pub(crate) const UTC_OFFSET: &str = "utc_offset";

/// The UTC offset to store beside a row's `ts` (#766, 1.13): the process's offset at
/// that instant ([`local_offset`]). `None` when `ts` is not a number or the C library
/// cannot place it; then no offset is stored. Called only where a row is appended.
pub(crate) fn offset_at(ts: &Value) -> Option<Value> {
    let t = ts.as_f64()?.floor() as i64;
    local_offset(t).map(Value::from)
}

/// WHEN AN ARRIVING MESSAGE IS WRITTEN: its `ts` and the UTC offset in force then, taken
/// once, before the assembly reads it ([`crate::assembly::assemble_messages`]), so the
/// request and the row it becomes keep the same ones.
#[derive(Clone, Debug)]
pub(crate) struct Stamp {
    /// Epoch seconds.
    pub(crate) ts: Value,
    /// Seconds east of UTC ([`offset_at`]); none when it cannot be placed.
    pub(crate) offset: Option<Value>,
}

impl Stamp {
    /// The stamp of now.
    #[must_use]
    pub(crate) fn now() -> Self {
        let ts = json!(crate::helpers::now_secs());
        let offset = offset_at(&ts);
        Self { ts, offset }
    }

    /// `row` with this `ts` and offset in its `attributes`.
    pub(crate) fn apply(&self, row: &mut Value) {
        row["attributes"]["ts"] = self.ts.clone();
        if let Some(off) = &self.offset {
            row["attributes"][UTC_OFFSET] = off.clone();
        }
    }

    /// The stamp as the environment section reads a thread's first row.
    #[must_use]
    pub(crate) fn env(&self) -> Option<crate::model_format::map::Env> {
        let mut row = json!({});
        self.apply(&mut row);
        crate::model_format::map::Env::of(&row)
    }
}

/// `t` (epoch seconds) read on a clock `offset` seconds ahead of UTC: its date
/// (`2026-11-19`) and the offset (`+02:00`). The process's time zone is never read here.
#[must_use]
pub(crate) fn day_of(t: i64, offset: i64) -> (String, String) {
    let local = t + offset;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let sign = if offset < 0 { '-' } else { '+' };
    let off = offset.abs() / 60;
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!("{sign}{:02}:{:02}", off / 60, off % 60),
    )
}

/// The kernel's UTC offset at the instant `t` (epoch seconds), in seconds: its local
/// clock reading less the UTC one. `None` when the C library cannot place `t`.
pub(crate) fn local_offset(t: i64) -> Option<i64> {
    let secs = t as libc::time_t;
    // SAFETY: `tm` is plain integers (and, on unix, a zone name pointer the call
    // sets); all zeros is a valid value of it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals for the length of the call.
    #[cfg(unix)]
    let placed = !unsafe { libc::localtime_r(&secs, &mut tm) }.is_null();
    // SAFETY: as above.
    #[cfg(windows)]
    let placed = unsafe { libc::localtime_s(&mut tm, &secs) } == 0;
    if !placed {
        return None;
    }
    let local = days_from_civil(
        i64::from(tm.tm_year) + 1900,
        i64::from(tm.tm_mon) + 1,
        i64::from(tm.tm_mday),
    ) * 86_400
        + i64::from(tm.tm_hour) * 3600
        + i64::from(tm.tm_min) * 60
        + i64::from(tm.tm_sec);
    Some(local - t)
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date `(year, month, day)` of a day count since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

/// The size cap of a row's `attributes` at write, parts excluded (they are bounded by
/// the delta / checkpoint design): an oversized map is refused.
pub(crate) const ATTRIBUTES_CAP: usize = 8 * 1024;

/// What the chat remembers of its previous request, in RAM: lost on a restart and
/// cleared by a compaction, and the next request is then a checkpoint.
#[derive(Clone, Debug)]
pub(crate) struct PrevRequest {
    /// The assistant row that request earned.
    pub(crate) row_id: i64,
    /// Its full parts list.
    pub(crate) list: Vec<Value>,
    /// Requests since the last checkpoint (the checkpoint itself is 0).
    pub(crate) n: u64,
    /// The running totals after it.
    pub(crate) totals: Value,
}

/// A short stable hash of `text`: the first 16 hex digits of its SHA-256.
pub(crate) fn short_hash(text: &str) -> String {
    short_hash_bytes(text.as_bytes())
}

/// [`short_hash`] of raw bytes (an image's).
pub(crate) fn short_hash_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The digest of a full parts list.
pub(crate) fn digest(list: &[Value]) -> String {
    short_hash(&Value::Array(list.to_vec()).to_string())
}

/// The identity of each entry of a list: `{part, ref?, n?}`. A row is its `ref`; a
/// system piece is its name, `n` counting a name that comes again (a prompt wrapped by
/// two `system_text` pieces).
fn idents(list: &[Value]) -> Vec<Value> {
    let mut seen: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    list.iter()
        .map(|e| {
            let mut id = Map::new();
            id.insert("part".into(), e["part"].clone());
            if let Some(r) = e.get("ref") {
                id.insert("ref".into(), r.clone());
            } else {
                let c = seen.entry(e["part"].to_string()).or_insert(0);
                if *c > 0 {
                    id.insert("n".into(), json!(*c));
                }
                *c += 1;
            }
            Value::Object(id)
        })
        .collect()
}

/// The delta that turns `prev` into `full`, or `None` when it cannot say it (the
/// entries both lists keep come in another order): the caller writes a checkpoint.
fn delta(prev_id: i64, prev: &[Value], full: &[Value]) -> Option<Value> {
    let (pi, fi) = (idents(prev), idents(full));
    let key = |v: &Value| v.to_string();
    let prev_at: std::collections::HashMap<String, usize> =
        pi.iter().enumerate().map(|(i, v)| (key(v), i)).collect();
    let full_keys: std::collections::HashSet<String> = fi.iter().map(key).collect();
    let kept_prev: Vec<String> = pi
        .iter()
        .map(key)
        .filter(|k| full_keys.contains(k))
        .collect();
    let kept_full: Vec<String> = fi
        .iter()
        .map(key)
        .filter(|k| prev_at.contains_key(k))
        .collect();
    if kept_prev != kept_full {
        return None;
    }
    let (mut add, mut change) = (Vec::new(), Vec::new());
    for (i, e) in full.iter().enumerate() {
        match prev_at.get(&key(&fi[i])) {
            None => {
                let mut a = e.clone();
                a["at"] = json!(i);
                add.push(a);
            }
            Some(&j) => {
                let old = &prev[j];
                let mut c = fi[i].as_object().cloned().unwrap_or_default();
                let mut changed = false;
                for field in ["form", "hash", "bytes"] {
                    if e.get(field) != old.get(field) {
                        if let Some(v) = e.get(field) {
                            c.insert(field.into(), v.clone());
                            changed = true;
                        }
                    }
                }
                if changed {
                    change.push(Value::Object(c));
                }
            }
        }
    }
    let drop: Vec<Value> = pi
        .iter()
        .filter(|v| !full_keys.contains(&key(v)))
        .cloned()
        .collect();
    Some(json!({
        "prev": prev_id,
        "add": add,
        "change": change,
        "drop": drop,
        "digest": digest(full),
    }))
}

/// The full list a `parts` value stands for, given the list it is a delta against
/// (ignored by a checkpoint).
#[cfg(test)]
pub(crate) fn apply(prev: &[Value], parts: &Value) -> Vec<Value> {
    if parts["full"] == json!(true) {
        return parts["list"].as_array().cloned().unwrap_or_default();
    }
    let drops: Vec<String> = parts["drop"]
        .as_array()
        .map(|a| a.iter().map(Value::to_string).collect())
        .unwrap_or_default();
    let changes: Vec<&Value> = parts["change"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let mut out: Vec<Value> = Vec::new();
    for (e, id) in prev.iter().zip(idents(prev)) {
        if drops.contains(&id.to_string()) {
            continue;
        }
        let mut e = e.clone();
        if let Some(c) = changes.iter().find(|c| {
            let mut cid: Value = (**c).clone();
            if let Some(m) = cid.as_object_mut() {
                m.retain(|k, _| matches!(k.as_str(), "part" | "ref" | "n"));
            }
            cid == id
        }) {
            for field in ["form", "hash", "bytes"] {
                if let Some(v) = c.get(field) {
                    e[field] = v.clone();
                }
            }
        }
        out.push(e);
    }
    for a in parts["add"].as_array().into_iter().flatten() {
        let at = a["at"].as_u64().unwrap_or(0) as usize;
        let mut e = a.clone();
        if let Some(m) = e.as_object_mut() {
            m.remove("at");
        }
        out.insert(at.min(out.len()), e);
    }
    out
}

/// The running totals after this request: `base` (the previous row's, or empty) plus
/// this request's bytes per part, and the fold counts read off what changed since the
/// previous list: `folds` (a row that was whole is now reduced), `recalls` (a reduced
/// row is whole again), `refolds` (a reduced row is reduced another way).
fn next_totals(base: &Value, prev: Option<&[Value]>, full: &[Value]) -> Value {
    let count = |k: &str| base.get(k).and_then(Value::as_u64).unwrap_or(0);
    let mut bytes: Map<String, Value> = base
        .get("bytes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for e in full {
        let name = e["part"].as_str().unwrap_or("").to_string();
        let sum = bytes.get(&name).and_then(Value::as_u64).unwrap_or(0)
            + e["bytes"].as_u64().unwrap_or(0);
        bytes.insert(name, json!(sum));
    }
    let (mut folds, mut recalls, mut refolds) =
        (count("folds"), count("recalls"), count("refolds"));
    if let Some(prev) = prev {
        let form_of: std::collections::HashMap<String, &str> = idents(prev)
            .iter()
            .zip(prev)
            .filter_map(|(id, e)| Some((id.to_string(), e.get("form")?.as_str()?)))
            .collect();
        for (id, e) in idents(full).iter().zip(full) {
            let (Some(old), Some(new)) = (
                form_of.get(&id.to_string()),
                e.get("form").and_then(Value::as_str),
            ) else {
                continue;
            };
            match (*old == "whole", new == "whole") {
                (true, false) => folds += 1,
                (false, true) => recalls += 1,
                (false, false) if *old != new => refolds += 1,
                _ => {}
            }
        }
    }
    json!({
        "requests": count("requests") + 1,
        "bytes": bytes,
        "folds": folds,
        "recalls": recalls,
        "refolds": refolds,
    })
}

/// The `parts` and `totals` of the request `full`, which the row `row_id` earns, and
/// what the chat remembers of it. A checkpoint (a full list) on the chat's first
/// request, the first after a restart or a compaction (`prev` is `None`), and every
/// [`CHECKPOINT_EVERY`] requests. `seed` is the totals of the last stored assistant row
/// when `prev` is `None` (a restart keeps counting).
pub(crate) fn build(
    prev: Option<&PrevRequest>,
    seed: Option<Value>,
    row_id: i64,
    full: Vec<Value>,
) -> (Value, Value, PrevRequest) {
    let base = prev
        .map(|p| p.totals.clone())
        .or(seed)
        .unwrap_or_else(|| json!({}));
    let totals = next_totals(&base, prev.map(|p| p.list.as_slice()), &full);
    let step = prev
        .filter(|p| p.n + 1 < CHECKPOINT_EVERY)
        .and_then(|p| delta(p.row_id, &p.list, &full).map(|d| (d, p.n + 1)));
    let (parts, n) = step.unwrap_or_else(|| {
        (
            json!({"full": true, "list": full, "digest": digest(&full)}),
            0,
        )
    });
    let next = PrevRequest {
        row_id,
        list: full,
        n,
        totals: totals.clone(),
    };
    (parts, totals, next)
}

/// The `attributes` of the newest assistant row among `rows` (the history a pass read),
/// if it has one.
fn newest_attributes(rows: &[Value]) -> Option<&Value> {
    rows.iter()
        .rev()
        .find(|r| r.get("role").and_then(Value::as_str) == Some("assistant"))?
        .get("attributes")
}

/// The totals of the newest assistant row among `rows`, if it has one.
pub(crate) fn seed_totals(rows: &[Value]) -> Option<Value> {
    newest_attributes(rows)?.get("totals").cloned()
}

/// The token counts since the chat's first pass, as the newest assistant row among `rows`
/// keeps them (`used.running`, #766 1.7), if it has them: a restart keeps counting.
pub(crate) fn seed_used(rows: &[Value]) -> Option<crate::used::Used> {
    crate::used::Used::from_value(newest_attributes(rows)?.get("used")?.get("running")?)
}

/// `model`'s entry in a provider record's `prices`: its own, else the entry of the id
/// it is a dated form of (the longest).
fn price_entry<'a>(meta: &'a Map<String, Value>, model: &str) -> Option<&'a Value> {
    let prices = meta.get("prices")?.as_object()?;
    prices.get(model).or_else(|| {
        prices
            .iter()
            .filter(|(id, _)| {
                model
                    .strip_prefix(id.as_str())
                    .is_some_and(|rest| rest.starts_with('-'))
            })
            .max_by_key(|(id, _)| id.len())
            .map(|(_, v)| v)
    })
}

/// The cost of one row's pass, priced from its counts (`used.pass.total`, #766 1.7):
/// `{usd, prices}`, the price entry of the serving provider's record kept with it (USD
/// per million tokens: `input` for `prompt`, `output` for `completion`, `cache_read`,
/// and `cache_write_5m` for `cache_write`), or `{unpriced: true}` when a price a
/// non-zero count needs is missing: never 0. `None` when the pass counted nothing.
pub(crate) fn cost(
    counts: Option<&crate::used::Counts>,
    provider_meta: &Map<String, Value>,
    model: &str,
) -> Option<Value> {
    let counts = counts.filter(|c| !c.is_zero())?;
    let unpriced = json!({"unpriced": true});
    let Some(entry) = price_entry(provider_meta, model) else {
        return Some(unpriced);
    };
    let p = |k: &str| entry.get(k).and_then(Value::as_f64);
    // a count of 0 costs nothing, whatever the entry prices
    let paid = |count: i64, price: Option<f64>| match count {
        0 => Some(0.0),
        c => price.map(|p| c as f64 * p),
    };
    let sum = (|| {
        let input = paid(counts.prompt, p("input"))?;
        let output = paid(counts.completion, p("output"))?;
        let read = paid(counts.cache_read, p("cache_read"))?;
        let write = paid(counts.cache_write, p("cache_write_5m"))?;
        Some((input + output + read + write) / 1e6)
    })();
    Some(match sum {
        Some(usd) => json!({"usd": usd, "prices": entry}),
        None => unpriced,
    })
}

/// `row` with `attributes.source` set to `source` (the agent that sent it; a peer
/// kernel's agent keeps its `<guid>|<id>` form).
pub(crate) fn with_source(mut row: Value, source: &str) -> Value {
    if source.is_empty() {
        return row;
    }
    if let Some(m) = row.as_object_mut() {
        let a = m.entry("attributes").or_insert_with(|| json!({}));
        if let Some(a) = a.as_object_mut() {
            a.insert("source".into(), json!(source));
        }
    }
    row
}

/// Refuses a row whose `attributes` (parts excluded) pass [`ATTRIBUTES_CAP`]; the
/// error names the agent's store, the row and the field.
pub(crate) fn check_cap(self_id: &AgentId, row: &Map<String, Value>) -> Result<(), String> {
    let Some(a) = row.get("attributes").and_then(Value::as_object) else {
        return Ok(());
    };
    let size: usize = a
        .iter()
        .filter(|(k, _)| k.as_str() != "parts")
        .map(|(k, v)| k.len() + v.to_string().len())
        .sum();
    if size > ATTRIBUTES_CAP {
        return Err(format!(
            "save_history: the attributes of row {} in the store of {} are {size} bytes, over the cap of {ATTRIBUTES_CAP} (parts excluded)",
            row.get("id").map_or_else(|| "?".to_string(), Value::to_string),
            self_id.as_str()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-19 19:40:00 UTC.
    const T: i64 = 1_795_117_200;

    #[test]
    fn the_day_is_the_local_date_with_its_offset() {
        assert_eq!(day_of(T, 7200), ("2026-11-19".into(), "+02:00".into()));
        assert_eq!(day_of(T, 0), ("2026-11-19".into(), "+00:00".into()));
        assert_eq!(
            day_of(T, -5 * 3600 - 1800),
            ("2026-11-19".into(), "-05:30".into())
        );
        // past local midnight: the next day
        assert_eq!(
            day_of(T, 5 * 3600 + 2700),
            ("2026-11-20".into(), "+05:45".into())
        );
        // the last second of a day is that day, never rounded into the next
        assert_eq!(day_of(951_782_400 - 1, 0).0, "2000-02-28");
        assert_eq!(day_of(951_782_400 + 59, 0).0, "2000-02-29");
        assert_eq!(day_of(-60, 0).0, "1969-12-31");
        assert_eq!(day_of(1_772_323_200 - 1, 0).0, "2026-02-28");
        // west of UTC, a UTC morning is the evening before
        assert_eq!(day_of(1_772_323_200 + 3600, -7200).0, "2026-02-28");
    }

    #[test]
    fn the_civil_date_round_trips() {
        for days in [-719_468, -1, 0, 1, 10_957, 11_016, 20_411, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
    }

    /// The offset is the kernel's own: what `date +%z` says for the same moment.
    #[cfg(unix)]
    #[test]
    fn the_offset_is_the_kernels_local_one() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let out = std::process::Command::new("date")
            .arg("+%z")
            .output()
            .expect("date runs");
        let z = String::from_utf8(out.stdout).unwrap();
        let z = z.trim();
        let sign = if z.starts_with('-') { -1 } else { 1 };
        let h: i64 = z[1..3].parse().unwrap();
        let m: i64 = z[3..5].parse().unwrap();
        assert_eq!(
            local_offset(now),
            Some(sign * (h * 3600 + m * 60)),
            "date +%z = {z}"
        );
    }
}
