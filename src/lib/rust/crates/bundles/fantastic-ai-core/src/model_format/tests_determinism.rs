//! NO FLAKY MAPPINGS (#766, section 2.10): the same input always maps to the same output.
//! The generator is seeded and written here; a failing seed is printed.

use super::rules::{shipped, type_of};

/// A seeded generator (splitmix64): the same seed gives the same sequence everywhere.
pub(super) struct Seeded(u64);

impl Seeded {
    pub(super) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(super) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `n` (`n > 0`).
    pub(super) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub(super) fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// `n` model ids from `seed`: rule substrings and look-alikes between provider prefixes,
/// sizes and tags, with the case of each letter flipped at random.
fn ids(seed: u64, n: usize) -> Vec<String> {
    let rules = shipped();
    let mut frags: Vec<String> = rules.rules.iter().map(|r| r.contains.clone()).collect();
    frags.extend(
        [
            "qwen",
            "qwen3",
            "qwen2.5",
            "qwen2",
            "qwe",
            "wen3",
            "coder",
            "hermes",
            "herme",
            "llama-3.1",
            "gpt-oss",
            "mistral",
            "distill",
            "vl",
            "math",
            "модель",
            "ї",
        ]
        .map(str::to_string),
    );
    let prefixes = [
        "",
        "Qwen/",
        "NousResearch/",
        "openrouter/qwen/",
        "hf.co/",
        "ollama/",
    ];
    let tails = [
        "",
        "-7b",
        "-8b",
        ":8b",
        "-30b-a3b",
        "-instruct",
        "-2507",
        "-awq",
        "@q4_k_m",
        " ",
    ];
    let mut g = Seeded::new(seed);
    (0..n)
        .map(|_| {
            let mut s = String::from(*g.pick(&prefixes));
            for _ in 0..=g.below(3) {
                s.push_str(g.pick(&frags));
                s.push_str(g.pick(&tails));
            }
            s.chars()
                .map(|c| {
                    if g.below(2) == 0 {
                        c.to_uppercase().collect::<String>()
                    } else {
                        c.to_string()
                    }
                })
                .collect()
        })
        .collect()
}

/// The types of `ids`, in order.
fn types(ids: &[String]) -> Vec<&'static str> {
    let rules = shipped();
    ids.iter().map(|id| type_of(rules, id)).collect()
}

// D9
#[test]
fn type_of_is_a_function() {
    const SEED: u64 = 0x0766_D9D9;
    let mut all: Vec<String> = super::tests_rules::real_ids()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    all.extend(ids(SEED, 10_000));
    let first = types(&all);
    // the table's own types
    for ((id, want), got) in super::tests_rules::real_ids().iter().zip(&first) {
        assert_eq!(*got, want.as_str(), "seed {SEED:#x}: {id}");
    }
    // repeated
    for round in 0..3 {
        assert_eq!(types(&all), first, "seed {SEED:#x}: round {round} differs");
    }
    // the same ids generated again
    assert_eq!(
        ids(SEED, 10_000),
        all[all.len() - 10_000..],
        "seed {SEED:#x}: generator drifted"
    );
    // on 8 threads
    let all = std::sync::Arc::new(all);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let all = std::sync::Arc::clone(&all);
            std::thread::spawn(move || types(&all))
        })
        .collect();
    for (i, h) in handles.into_iter().enumerate() {
        let got = h.join().expect("thread");
        for (j, (a, b)) in got.iter().zip(&first).enumerate() {
            assert_eq!(a, b, "seed {SEED:#x}: thread {i}, id {:?}", all[j]);
        }
    }
}

// ─── the row's stored UTC offset (1.13) ─────────────────────────────────────────

use crate::row_attrs::{head, local_offset, model_text, offset_at, stamp, UTC_OFFSET};
use crate::tests::{mk_kernel, pass, script, until_true};
use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// 2026-11-19 19:40:00 UTC: PST in Los Angeles (-08:00), +14:00 on Kiritimati.
const T: i64 = 1_795_117_200;

/// The variable that tells a child run of this binary which side of a test it is.
const CHILD_ROLE: &str = "FANTASTIC_D_CHILD_ROLE";
/// The variable that names the file a child reads.
const CHILD_INPUT: &str = "FANTASTIC_D_CHILD_INPUT";

const GO: &str = r#"ok <call name="done"></call>"#;

/// The role this process runs as, when it is a child of a test.
fn child_role() -> Option<String> {
    std::env::var(CHILD_ROLE).ok()
}

/// Run `test` of this module again in a child process of the test binary, as `role`,
/// with `envs` set and `input` named; panics unless the child ran it and it passed.
fn run_child(
    test: &str,
    role: &str,
    envs: &[(&str, &str)],
    cwd: Option<&std::path::Path>,
    input: Option<&std::path::Path>,
) {
    let module = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, m)| m);
    let mut c = std::process::Command::new(std::env::current_exe().expect("the test binary"));
    c.args([
        format!("{module}::{test}").as_str(),
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD_ROLE, role);
    if let Some(p) = input {
        c.env(CHILD_INPUT, p);
    }
    for (k, v) in envs {
        c.env(k, v);
    }
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let out = c.output().expect("the child runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "child {role} {envs:?}:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One environment of D10: (role, variables, another cwd).
type Env = (&'static str, Vec<(&'static str, &'static str)>, bool);

/// The environments of D10 a child runs under: (role, variables, another cwd).
fn d10_zones() -> Vec<Env> {
    vec![
        ("lc_c", vec![("LC_ALL", "C")], false),
        ("lc_uk", vec![("LC_ALL", "uk_UA.UTF-8")], false),
        ("tz_kiritimati", vec![("TZ", "Pacific/Kiritimati")], false),
        ("tz_los_angeles", vec![("TZ", "America/Los_Angeles")], false),
        ("cwd", vec![], true),
    ]
}

/// In a child under a `TZ`, the zone took: the process's own offset at [`T`] is the
/// zone's, so a head that equals the parent's was not read from the process.
fn zone_took(role: &str) {
    match role {
        "tz_kiritimati" => assert_eq!(local_offset(T), Some(50_400), "{role}"),
        "tz_los_angeles" => assert_eq!(local_offset(T), Some(-28_800), "{role}"),
        _ => {}
    }
}

/// Every row of the agent in its store, bookkeeping included.
async fn store(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"].as_array().cloned().unwrap_or_default()
}

/// The stored row with `id`.
fn by_id(rows: &[Value], id: i64) -> Value {
    rows.iter()
        .find(|r| r["id"] == json!(id))
        .cloned()
        .unwrap_or_else(|| panic!("no row {id} in {rows:#?}"))
}

/// The stored row whose content is `content`.
fn by_content(rows: &[Value], content: &str) -> Value {
    rows.iter()
        .find(|r| r["content"] == json!(content))
        .cloned()
        .unwrap_or_else(|| panic!("no row {content:?} in {rows:#?}"))
}

/// One verb sent to the agent; its result.
async fn verb(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    kernel
        .operator_send(vec![Item::new(agent.clone(), payload)])
        .await
        .remove(0)
        .result
}

/// A send, once the last chain has ended (each send here is its own chain).
async fn say(kernel: &Arc<Kernel>, agent: &AgentId, text: &str) {
    until_true("the chain to end", || {
        !crate::state::state_for(agent).chain.lock().unwrap().alive
    })
    .await;
    let r = verb(kernel, agent, json!({"type": "send", "text": text})).await;
    assert!(r.get("error").is_none(), "{r}");
    until_true("the chain to end", || {
        !crate::state::state_for(agent).chain.lock().unwrap().alive
    })
    .await;
}

/// The turn request whose new input is `said` (a recall's side call is not one).
fn turn_request(s: &crate::tests::MockScript, said: &str) -> Vec<Value> {
    (0..s.passes_served())
        .map(|n| s.request_messages(n))
        .find(|req| {
            req.last()
                .and_then(|m| m["content"].as_str())
                .is_some_and(|c| crate::row_attrs::without_head(c) == said)
        })
        .unwrap_or_else(|| panic!("no turn request for {said:?}"))
}

/// `rows` saved through the chat's append ([`crate::memory::save_history`]); their ids.
async fn save(kernel: &Arc<Kernel>, agent: &AgentId, rows: &[Value]) -> Vec<i64> {
    as_agent(
        agent.clone(),
        crate::memory::save_history(agent, kernel, rows),
    )
    .await
    .expect("rows saved")
}

/// The old rows of D13: a `ts` and no `utc_offset`, or one that is not an integer.
fn old_rows() -> Vec<Value> {
    let offsets = [
        None,
        Some(json!("7200")),
        Some(json!(7200.5)),
        Some(Value::Null),
    ];
    let roles = ["user", "assistant", "user", "assistant"];
    offsets
        .iter()
        .zip(roles)
        .enumerate()
        .map(|(i, (off, role))| {
            let mut attrs = json!({"ts": T, "source": "lead"});
            if let Some(o) = off {
                attrs[UTC_OFFSET] = o.clone();
            }
            json!({"role": role, "content": format!("old-row-{i}"), "attributes": attrs})
        })
        .collect()
}

// D13
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_row_renders_utc() {
    let want = format!("{}\n", stamp(T, 0));
    assert_eq!(want, "[2026-11-19 19:40 +00:00]\n");
    if let Some(role) = child_role() {
        zone_took(&role);
        for row in old_rows() {
            assert_eq!(head(&row), want, "{role}: {row}");
        }
        return;
    }
    for row in old_rows() {
        assert_eq!(head(&row), want, "{row}");
    }
    // under every zone of D10
    let elsewhere = tempfile::TempDir::new().unwrap();
    for (role, envs, cwd) in d10_zones() {
        run_child(
            "old_row_renders_utc",
            role,
            &envs,
            cwd.then(|| elsewhere.path()),
            None,
        );
    }

    // NO BACKFILL: rows stored before the change (appended to the store as they were,
    // not through the chat's append) keep their attributes through passes and a restart
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "d13_old").await;
    // spare passes: a recall after a restart may ask the provider first
    let s = script(&agent, (0..8).map(|_| pass(GO)).collect(), Duration::ZERO);
    let mut old = old_rows();
    for r in &mut old {
        r["id"] = json!(kernel.mint_id());
    }
    let res = as_agent(
        agent.clone(),
        crate::memory::store_call(&agent, &kernel, json!({"type": "append", "rows": old})),
    )
    .await
    .expect("store append");
    assert!(res.get("error").is_none(), "{res:?}");
    // the restart that reads them from the store
    let _ = verb(&kernel, &agent, json!({"type": "boot"})).await;
    say(&kernel, &agent, "first").await;
    let _ = verb(&kernel, &agent, json!({"type": "boot"})).await;
    say(&kernel, &agent, "second").await;
    let rows = store(&kernel, &agent).await;
    for r in &old {
        let id = r["id"].as_i64().unwrap();
        assert_eq!(
            by_id(&rows, id)["attributes"],
            r["attributes"],
            "row {id} was rewritten"
        );
    }
    // and each turn's request shows every old row under the UTC head
    for said in ["first", "second"] {
        let sent = turn_request(&s, said);
        for r in &old {
            let text = format!("{want}{}", r["content"].as_str().unwrap());
            assert!(
                sent.iter()
                    .any(|m| m["content"].as_str().is_some_and(|c| c.contains(&text))),
                "the request of {said:?} lacks {text:?}: {sent:#?}"
            );
        }
    }
}

// D14
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offset_set_once_on_append() {
    if let Some(role) = child_role() {
        // a restart under another zone: the stored rows' heads are the writer's
        zone_took(&role);
        let path = std::env::var(CHILD_INPUT).expect("an input file");
        let input: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(path).expect("input")).expect("json");
        for e in &input {
            assert_eq!(json!(head(&e["row"])), e["head"], "{role}: {}", e["row"]);
        }
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "d14_once").await;
    // spare passes: a recall may ask the provider before a turn
    let s = script(&agent, (0..12).map(|_| pass(GO)).collect(), Duration::ZERO);

    // A ROW SAVED WITH NO OFFSET gets the writing process's offset at its `ts`
    let ids = save(
        &kernel,
        &agent,
        &[
            json!({"role": "user", "content": "plain", "attributes": {"ts": T}}),
            json!({"role": "assistant", "content": "own", "attributes": {"ts": T, UTC_OFFSET: 31_500}}),
        ],
    )
    .await;
    let rows = store(&kernel, &agent).await;
    let plain = by_id(&rows, ids[0]);
    assert_eq!(
        plain["attributes"].get(UTC_OFFSET).cloned(),
        offset_at(&json!(T)),
        "{plain}"
    );
    assert_eq!(
        head(&plain),
        format!("{}\n", stamp(T, local_offset(T).unwrap()))
    );
    // A ROW SAVED WITH ONE keeps its own: as written, as a copy, as a row saved again
    let own = by_id(&rows, ids[1]);
    assert_eq!(own["attributes"][UTC_OFFSET], json!(31_500), "{own}");
    assert_eq!(head(&own), "[2026-11-20 04:25 +08:45]\n");
    let mut copy = own.clone();
    copy.as_object_mut().unwrap().remove("id");
    copy["content"] = json!("own, copied");
    let copied = save(&kernel, &agent, &[copy]).await[0];
    let first = save(
        &kernel,
        &agent,
        &[json!({"role": "user", "content": "saved twice", "attributes": {"ts": T, UTC_OFFSET: -34_200}})],
    )
    .await[0];
    let mut again = by_id(&store(&kernel, &agent).await, first);
    again.as_object_mut().unwrap().remove("id");
    again["content"] = json!("saved twice, again");
    let resaved = save(&kernel, &agent, &[again]).await[0];
    let rows = store(&kernel, &agent).await;
    assert_eq!(
        by_id(&rows, copied)["attributes"][UTC_OFFSET],
        json!(31_500)
    );
    assert_eq!(
        by_id(&rows, first)["attributes"][UTC_OFFSET],
        json!(-34_200)
    );
    assert_eq!(
        by_id(&rows, resaved)["attributes"][UTC_OFFSET],
        json!(-34_200)
    );

    // THE ARRIVING MESSAGE: the head its request showed is its row's, on the next pass
    say(&kernel, &agent, "go").await;
    say(&kernel, &agent, "again").await;
    let rows = store(&kernel, &agent).await;
    let go = by_content(&rows, "go");
    let ts = go["attributes"]["ts"].clone();
    assert_eq!(
        go["attributes"].get(UTC_OFFSET).cloned(),
        offset_at(&ts),
        "{go}"
    );
    let shown = model_text(&go);
    assert_eq!(
        turn_request(&s, "go").last().unwrap()["content"],
        json!(shown)
    );
    let next = turn_request(&s, "again");
    assert!(
        next.iter().any(|m| m["content"] == json!(shown)),
        "the next pass shows another head: {next:#?}"
    );
    // its answer, stored through the append, has one too
    let answer = rows
        .iter()
        .rev()
        .find(|r| r["role"] == "assistant")
        .unwrap();
    assert_eq!(
        answer["attributes"].get(UTC_OFFSET).cloned(),
        offset_at(&answer["attributes"]["ts"]),
        "{answer}"
    );

    // A FOLD ROW: its offset is set where its `ts` is, and the next pass shows its head
    let out = verb(
        &kernel,
        &agent,
        json!({"type": "context_fold", "from": ids[0], "to": resaved, "summary": "FOLD-D14-ONE"}),
    )
    .await;
    let folded = out["folded"]
        .as_i64()
        .unwrap_or_else(|| panic!("context_fold refused: {out}"));
    say(&kernel, &agent, "after the fold").await;
    let rows = store(&kernel, &agent).await;
    let fold = by_id(&rows, folded);
    assert_eq!(
        fold["attributes"].get(UTC_OFFSET).cloned(),
        offset_at(&fold["attributes"]["ts"]),
        "{fold}"
    );
    let fold_head = head(&fold);
    assert!(!fold_head.is_empty(), "{fold}");
    let sent = turn_request(&s, "after the fold");
    let shown = sent
        .iter()
        .find(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("FOLD-D14-ONE"))
        })
        .unwrap_or_else(|| panic!("the fold is not in the request: {sent:#?}"));
    assert!(
        shown["content"].as_str().unwrap().starts_with(&fold_head),
        "{shown} under {fold_head:?}"
    );

    // NO LATER WRITE CHANGES IT: a fold, a recall, a view write and a restart over them
    let before: Vec<(i64, Option<Value>)> = rows
        .iter()
        .filter_map(|r| Some((r["id"].as_i64()?, r["attributes"].get(UTC_OFFSET).cloned())))
        .collect();
    let again_row = by_content(&rows, "again");
    let again_answer = rows
        .iter()
        .find(|r| r["role"] == "assistant" && r["id"].as_i64() > again_row["id"].as_i64())
        .unwrap();
    let out = verb(
        &kernel,
        &agent,
        json!({"type": "context_fold", "from": again_row["id"], "to": again_answer["id"],
               "summary": "FOLD-D14-TWO"}),
    )
    .await;
    assert!(out["folded"].is_i64(), "context_fold refused: {out}");
    let out = verb(
        &kernel,
        &agent,
        json!({"type": "context_recall", "dry": true}),
    )
    .await;
    assert!(out.get("error").is_none(), "{out}");
    as_agent(
        agent.clone(),
        crate::memory::save_view(&agent, &kernel, &[], &[json!({"id": folded})], 1000, "d14"),
    )
    .await
    .expect("view saved");
    let _ = verb(&kernel, &agent, json!({"type": "boot"})).await;
    say(&kernel, &agent, "after the restart").await;
    let rows = store(&kernel, &agent).await;
    for (id, off) in &before {
        assert_eq!(
            by_id(&rows, *id)["attributes"].get(UTC_OFFSET).cloned(),
            *off,
            "row {id}'s offset changed"
        );
    }
    // and under another zone the stored rows show the heads the writer showed
    let input: Vec<Value> = rows
        .iter()
        .filter(|r| r["attributes"].get("ts").is_some())
        .map(|r| json!({"row": r, "head": head(r)}))
        .collect();
    let file = tmp.path().join("d14_rows.json");
    std::fs::write(&file, serde_json::to_string(&input).unwrap()).unwrap();
    for (role, envs) in [
        ("tz_kiritimati", [("TZ", "Pacific/Kiritimati")]),
        ("tz_los_angeles", [("TZ", "America/Los_Angeles")]),
    ] {
        run_child("offset_set_once_on_append", role, &envs, None, Some(&file));
    }
}

// ─── the recorded prefix files (section 3, Fixtures: W4's last step) ───────────

const HISTORY: &str = include_str!("res/test/history.json");

/// `res/test/history.json` rendered for one adapter: each row's model text (its `ts`
/// head included) through `render`, each under a `== row <id> <role> ==` line. What the
/// prefix files hold below their header (K29, D2, D10, RS12).
pub(super) fn rendered_history(renderer: &dyn super::Renderer) -> String {
    let rows: Vec<Value> = serde_json::from_str(HISTORY).expect("history.json");
    let mut out = String::new();
    for row in &rows {
        if !row.get("content").is_some_and(Value::is_string) {
            continue;
        }
        let id = row.get("id").map_or_else(String::new, Value::to_string);
        let role = row.get("role").and_then(Value::as_str).unwrap_or("");
        out.push_str(&format!("== row {id} {role} ==\n"));
        out.push_str(&renderer.render(&model_text(row)));
        out.push('\n');
    }
    out
}

/// The SHA-256 of the shipped rule file's bytes, in hex.
pub(super) fn rules_digest() -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(super::rules::RULES_JSON.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A recorded prefix file: the `rev` and the rule file digest of its header, and the
/// body below the header.
pub(super) fn recorded(file: &str) -> (u32, String, &str) {
    let mut rev = None;
    let mut digest = None;
    let mut at = 0usize;
    for line in file.split_inclusive('\n') {
        let Some(h) = line.strip_prefix("# ") else {
            break;
        };
        at += line.len();
        let h = h.trim_end();
        if let Some(v) = h.strip_prefix("rev: ") {
            rev = v.parse().ok();
        } else if let Some(v) = h.strip_prefix("rules_sha256: ") {
            digest = Some(v.to_string());
        }
    }
    (
        rev.expect("a `# rev:` line"),
        digest.expect("a `# rules_sha256:` line"),
        &file[at..],
    )
}

/// The commit a recording names: `FANTASTIC_RECORD_COMMIT` (a checkout with no `.git`,
/// as on the rig), else the checkout's `HEAD`.
fn record_commit() -> String {
    if let Ok(c) = std::env::var("FANTASTIC_RECORD_COMMIT") {
        return c;
    }
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || "unknown".to_string(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
        )
}

/// Writes `res/test/prefix_d.txt` and `prefix_h.txt` from `history.json`, in a process of
/// its own. Run by hand, only when an adapter's `rev()` or the rule file changes:
/// `cargo test -p fantastic-ai-core model_format::tests_determinism::record_prefix -- --ignored`.
#[test]
#[ignore = "records the prefix fixtures; run by hand (section 3, Fixtures)"]
fn record_prefix() {
    let commit = record_commit();
    for (kind, file) in [("default", "prefix_d.txt"), ("hermes", "prefix_h.txt")] {
        let e = super::of(kind);
        let text = format!(
            "# recorded by model_format::tests_determinism::record_prefix (#766): \
             res/test/history.json, each row's model text through the adapter\n\
             # adapter: {kind}\n# rev: {}\n# rules_sha256: {}\n# commit: {commit}\n{}",
            e.renderer.rev(),
            rules_digest(),
            rendered_history(e.renderer),
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/model_format/res/test")
            .join(file);
        std::fs::write(&path, &text).expect("the prefix file is written");
        let (rev, digest, body) = recorded(&text);
        assert_eq!(
            (rev, digest.as_str()),
            (e.renderer.rev(), rules_digest().as_str())
        );
        assert_eq!(body, rendered_history(e.renderer));
    }
}

// ─── D1-D8, D10-D12 (W11) ────────────────────────────────────────────────────

use super::cache::{epoch_of, RenderCache};
use super::default::Tags;
use super::hermes::Hermes;
use super::tests_cache::{plain, run, Opts};
use super::{Out, Reader, Renderer};
use crate::think::{THINK_CLOSE, THINK_OPEN};
use crate::tool_parse::grammar::ROUTING;
use crate::tool_parse::render_call;
use serde_json::Map;

/// The variable that names the file a child writes.
const CHILD_OUTPUT: &str = "FANTASTIC_D_CHILD_OUTPUT";

/// Every registered adapter: `(renderer, reader)`.
fn adapters() -> Vec<(&'static dyn Renderer, &'static dyn Reader)> {
    super::REGISTRY
        .iter()
        .map(|e| (e.renderer, e.reader))
        .collect()
}

/// Whether this system has the time zone `tz` (its zoneinfo file).
pub(super) fn zone_known(tz: &str) -> bool {
    tz == "UTC"
        || [
            "/usr/share/zoneinfo",
            "/usr/lib/zoneinfo",
            "/var/db/timezone/zoneinfo",
        ]
        .iter()
        .any(|d| std::path::Path::new(d).join(tz).exists())
}

/// Whether this system has the locale `name` (`locale -a`, case and dash aside).
pub(super) fn locale_known(name: &str) -> bool {
    let norm = |s: &str| s.to_lowercase().replace('-', "");
    std::process::Command::new("locale")
        .arg("-a")
        .output()
        .ok()
        .is_some_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| norm(l.trim()) == norm(name))
        })
}

/// The environments of D10 this system has: (role, variables, another cwd). A locale or
/// zone it lacks is left out, and said so.
fn d10_envs() -> Vec<Env> {
    d10_zones()
        .into_iter()
        .filter(|(role, vars, _)| {
            let ok = vars.iter().all(|(k, v)| match *k {
                "TZ" => zone_known(v),
                "LC_ALL" => *v == "C" || locale_known(v),
                _ => true,
            });
            if !ok {
                eprintln!("D10: {role} {vars:?} is not on this system: skipped");
            }
            ok
        })
        .collect()
}

/// The internal text `reader` gives back for `pieces` fed in order: its texts and calls.
/// `Err`: a `Bad` or an `Open` came out.
fn read_pieces(reader: &dyn Reader, pieces: &[&str]) -> Result<String, String> {
    let mut o = reader.outbound();
    let mut outs = Vec::new();
    for p in pieces {
        outs.extend(o.feed(p));
    }
    outs.extend(o.finish());
    let mut text = String::new();
    for x in outs {
        match x {
            Out::Text(t) | Out::Call { text: t } => text.push_str(&t),
            other => return Err(format!("{other:?}")),
        }
    }
    Ok(text)
}

/// `text` rendered by `r` and read back by `reader` in one delta: the internal text.
fn round_trip(r: &dyn Renderer, reader: &dyn Reader, text: &str) -> String {
    let rendered = r.render(text);
    read_pieces(reader, &[&rendered])
        .unwrap_or_else(|e| panic!("{}: {e} reading {rendered:?}", r.name()))
}

/// A send in the internal form.
fn send_call(args: Value) -> String {
    render_call("send", args.as_object().expect("an object"))
}

/// The rows of `res/test/history.json` with string content, as a pass's view carries
/// them: `{id, role, content: model text}`, after a system message and before an
/// arriving one.
fn history_messages() -> Vec<Value> {
    let rows: Vec<Value> = serde_json::from_str(HISTORY).expect("history.json");
    let mut out = vec![json!({"role": "system", "content": "SYS"})];
    out.extend(
        rows.iter()
            .filter(|r| r.get("content").is_some_and(Value::is_string))
            .map(|r| json!({"id": r["id"], "role": r["role"], "content": model_text(r)})),
    );
    out.push(json!({"role": "user", "content": "go"}));
    out
}

// D1
/// One history rendered 100 times with the adapter values built anew each time, D and H:
/// byte-equal every time.
#[test]
fn repeated_passes() {
    let first_d = rendered_history(&Tags);
    let first_h = rendered_history(&Hermes);
    assert_ne!(first_d, first_h);
    for round in 0..100 {
        let d: Box<dyn Renderer> = Box::new(Tags);
        let h: Box<dyn Renderer> = Box::new(Hermes);
        assert_eq!(rendered_history(&*d), first_d, "D, round {round}");
        assert_eq!(rendered_history(&*h), first_h, "H, round {round}");
        assert_eq!(rendered_history(super::of("hermes").renderer), first_h);
    }
}

// D2
/// Two processes: a child run of this binary renders `history.json`, and its bytes equal
/// the parent's and the checked-in prefix files'; the same render on 8 threads, each
/// with maps built under its own hash seed, is byte-equal.
#[test]
fn other_process_same_bytes() {
    let d = rendered_history(&Tags);
    let h = rendered_history(&Hermes);
    if child_role().as_deref() == Some("d2") {
        let out = std::env::var(CHILD_OUTPUT).expect("an output file");
        std::fs::write(
            out,
            serde_json::to_string(&json!({"d": d, "h": h})).unwrap(),
        )
        .expect("written");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let file = tmp.path().join("d2.json");
    let path = file.to_string_lossy().to_string();
    run_child(
        "other_process_same_bytes",
        "d2",
        &[(CHILD_OUTPUT, path.as_str())],
        None,
        None,
    );
    let got: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(got["d"], json!(d), "D: the child's bytes");
    assert_eq!(got["h"], json!(h), "H: the child's bytes");
    // the checked-in recordings
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/model_format/res/test");
    for (body, file) in [(&d, "prefix_d.txt"), (&h, "prefix_h.txt")] {
        let text = std::fs::read_to_string(dir.join(file)).unwrap_or_else(|e| {
            panic!("{file}: recorded by W4's last step (section 3, Fixtures): {e}")
        });
        assert_eq!(recorded(&text).2, body.as_str(), "{file}");
    }
    // 8 threads, each looking its rows up in a map under its own hash seed
    let handles: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                let rows: Vec<Value> = serde_json::from_str(HISTORY).expect("history.json");
                let mut by_id: std::collections::HashMap<String, Value> =
                    std::collections::HashMap::with_hasher(
                        std::collections::hash_map::RandomState::new(),
                    );
                for r in &rows {
                    by_id.insert(r["id"].to_string(), r.clone());
                }
                let mut out = Vec::new();
                for (r, name) in [(&Tags as &dyn Renderer, "d"), (&Hermes, "h")] {
                    let mut text = String::new();
                    for id in rows.iter().map(|r| r["id"].to_string()) {
                        let row = &by_id[&id];
                        if !row.get("content").is_some_and(Value::is_string) {
                            continue;
                        }
                        let role = row.get("role").and_then(Value::as_str).unwrap_or("");
                        text.push_str(&format!("== row {id} {role} ==\n"));
                        text.push_str(&r.render(&model_text(row)));
                        text.push('\n');
                    }
                    out.push((name, text));
                }
                out
            })
        })
        .collect();
    for (i, t) in handles.into_iter().enumerate() {
        for (name, text) in t.join().expect("thread") {
            let want = if name == "d" { &d } else { &h };
            assert_eq!(&text, want, "thread {i}, {name}");
        }
    }
}

// D3
/// Map key order: one payload stored with its keys in two orders renders to its own
/// bytes each, the same every time; routing always comes out in `ROUTING` order; 1 000
/// payloads of 16 keys render equal on every thread.
#[test]
fn key_order() {
    // stored in the internal form the reader writes (`render_call`, 1.4), so the round
    // trip below is byte-exact
    let a = &send_call(json!({"target_id": "notes",
        "payload": {"type": "write", "path": "a.md", "bytes": "x"}}));
    let b = &send_call(json!({"target_id": "notes",
        "payload": {"path": "a.md", "type": "write", "bytes": "x"}}));
    let (ha, hb) = (Hermes.render(a), Hermes.render(b));
    assert_ne!(ha, hb, "each stored order is its own source");
    for _ in 0..10 {
        assert_eq!(Hermes.render(a), ha);
        assert_eq!(Hermes.render(b), hb);
    }
    let at = |s: &str, k: &str| {
        s.find(&format!("\"{k}\""))
            .unwrap_or_else(|| panic!("{k} in {s}"))
    };
    assert!(at(&ha, "type") < at(&ha, "path"), "{ha}");
    assert!(at(&hb, "path") < at(&hb, "type"), "{hb}");
    assert_eq!(read_pieces(&Hermes, &[&ha]).unwrap(), *a);
    assert_eq!(read_pieces(&Hermes, &[&hb]).unwrap(), *b);
    // routing stored in another order than `ROUTING`
    let routed = "<call name=\"send\"><routing><title>T</title><reply_to>self</reply_to>\
                  <target_id>notes</target_id></routing><payload><type>read</type></payload></call>";
    let hr = Hermes.render(routed);
    for _ in 0..10 {
        assert_eq!(Hermes.render(routed), hr);
    }
    let mut seen: Vec<(usize, &str)> = ["title", "reply_to", "target_id"]
        .iter()
        .map(|k| (at(&hr, k), *k))
        .collect();
    seen.sort_unstable();
    let order: Vec<&str> = seen.into_iter().map(|(_, k)| k).collect();
    let want: Vec<&str> = ROUTING
        .iter()
        .copied()
        .filter(|k| ["title", "reply_to", "target_id"].contains(k))
        .collect();
    assert_eq!(order, want, "{hr}");
    // 1 000 payloads of 16 keys, each in its own seeded order, on 8 threads
    let mut g = Seeded::new(0x0766_D3D3);
    let texts: Vec<String> = (0..1000)
        .map(|_| {
            let mut keys: Vec<usize> = (0..16).collect();
            for i in (1..keys.len()).rev() {
                keys.swap(i, g.below(i + 1));
            }
            let mut payload = Map::new();
            payload.insert("type".into(), json!("write"));
            for k in keys {
                payload.insert(
                    format!("k{k:02}"),
                    json!(format!("значення {}", g.below(1000))),
                );
            }
            send_call(json!({"target_id": "notes", "payload": payload}))
        })
        .collect();
    let once: Vec<Vec<String>> = adapters()
        .iter()
        .map(|(r, _)| texts.iter().map(|t| r.render(t)).collect())
        .collect();
    let texts = std::sync::Arc::new(texts);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let texts = std::sync::Arc::clone(&texts);
            std::thread::spawn(move || {
                adapters()
                    .iter()
                    .map(|(r, _)| texts.iter().map(|t| r.render(t)).collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for (i, h) in handles.into_iter().enumerate() {
        assert_eq!(h.join().expect("thread"), once, "thread {i}");
    }
}

// D4
/// Whitespace and trailing newlines, in prose, in a scalar and in a body: nothing outside
/// an envelope changes, and the same input renders to the same bytes twice. A body's
/// bytes are kept byte for byte through every round trip. A scalar comes back trimmed
/// through the hermes round trip, as `parse_call` trims it; the default adapter gives
/// back the stored text.
#[test]
fn whitespace_kept() {
    let prose = "  ведучі пробіли\tі таби  \r\nрядок\n\n\nкінець  ";
    let body = "a\r\n\tb  \n\n";
    let call = send_call(json!({"target_id": "notes", "title": "  назва\t ",
        "payload": {"type": "write", "path": " a.md ", "bytes": body}}));
    // the call as `parse_call` reads it: scalars trimmed, the body verbatim
    let trimmed = send_call(json!({"target_id": "notes", "title": "назва",
        "payload": {"type": "write", "path": "a.md", "bytes": body}}));
    let block = format!("<bytes>{body}</bytes>");
    for tail in ["", "\n", "\n\n\n", "\r\n"] {
        for (r, reader) in adapters() {
            let p = format!("{prose}{tail}");
            assert_eq!(r.render(&p), p, "{}: prose {tail:?}", r.name());
            assert_eq!(round_trip(r, reader, &p), p, "{}: prose {tail:?}", r.name());
            let text = format!("{prose}\n{call}{tail}");
            let rendered = r.render(&text);
            assert_eq!(r.render(&text), rendered, "{}: twice {tail:?}", r.name());
            assert!(
                rendered.starts_with(&format!("{prose}\n")),
                "{}: {rendered:?}",
                r.name()
            );
            assert!(rendered.ends_with(tail), "{}: {rendered:?}", r.name());
            assert!(rendered.contains(body), "{}: the body's bytes", r.name());
            let back = round_trip(r, reader, &text);
            assert!(back.contains(&block), "{}: the body's bytes back {back:?}", r.name());
            let want = if r.name() == Hermes.name() {
                format!("{prose}\n{trimmed}{tail}")
            } else {
                text.clone()
            };
            assert_eq!(back, want, "{}: {tail:?}", r.name());
        }
    }
    assert!(Hermes.render(&call).contains(&block));
}

// D5
/// Unicode: Ukrainian in NFC and NFD (`ї`, `й`), an emoji with a joiner, a BOM, U+2028.
/// Nothing is normalised: each form renders to its own bytes and round-trips; the H JSON
/// writes them unescaped.
#[test]
fn unicode_not_normalised() {
    let nfc = "ї й";
    let nfd = "і\u{0308} и\u{0306}";
    assert_ne!(nfc, nfd);
    let extra = "👩\u{200d}💻 \u{feff}ще\u{2028}рядок";
    for form in [nfc, nfd] {
        let call = send_call(
            json!({"target_id": "notes", "title": format!("{form} {extra}"),
            "payload": {"type": "write", "bytes": format!("{form}\n{extra}\n")}}),
        );
        let text = format!("{form} {extra}\n{call}");
        for (r, reader) in adapters() {
            let rendered = r.render(&text);
            assert!(
                rendered.starts_with(&format!("{form} {extra}\n")),
                "{}",
                r.name()
            );
            assert_eq!(round_trip(r, reader, &text), text, "{}: {form:?}", r.name());
        }
        let h = Hermes.render(&call);
        assert!(h.contains(&format!("\"{form} {extra}\"")), "unescaped: {h}");
        assert!(!h.contains("\\u"), "{h}");
    }
    let (a, b) = (format!("{nfc}\n"), format!("{nfd}\n"));
    for (r, _) in adapters() {
        assert_ne!(
            r.render(&a),
            r.render(&b),
            "{}: the two forms stay two",
            r.name()
        );
    }
}

/// The maximal runs of ASCII digits in `s`.
fn digit_runs(s: &str) -> Vec<&str> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|r| !r.is_empty())
        .collect()
}

// D6
/// Timestamps and ids: a `ts` head, row ids, pids in reply tags, a `client_id` in text are
/// copied as stored; the output holds no number that is not in its input; the same row
/// with two `ts` heads is two sources, each stable.
#[test]
fn ts_and_ids_copied() {
    let content = format!(
        "[reply to 11 · notes read · ↑4 · 1 of 2 in batch 7]\nc_7f3a готово\n\n\
         [reply to 12 · kernel reflect]\n{}",
        send_call(json!({"target_id": "notes", "payload": {"type": "read", "path": "n42.md"}}))
    );
    let row = |off: i64| {
        json!({"id": 41, "role": "user", "content": content,
               "attributes": {"ts": T, UTC_OFFSET: off}})
    };
    let (a, b) = (row(7200), row(-28_800));
    let (src_a, src_b) = (model_text(&a), model_text(&b));
    assert_ne!(src_a, src_b, "two heads, two sources");
    for (r, _) in adapters() {
        for src in [&src_a, &src_b] {
            let out = r.render(src);
            assert_eq!(r.render(src), out, "{}: stable", r.name());
            assert!(
                out.starts_with(&head(if src == &src_a { &a } else { &b })),
                "{}",
                r.name()
            );
            for needle in ["c_7f3a", "[reply to 11 ·", "[reply to 12 ·", "n42.md"] {
                assert!(out.contains(needle), "{}: {needle} in {out}", r.name());
            }
            for run in digit_runs(&out) {
                assert!(
                    src.contains(run),
                    "{}: {run} is not in the input: {out}",
                    r.name()
                );
            }
        }
    }
    // through a cache: each head is its own source, each stable
    let c = <RenderCache>::new();
    let view = |src: &str| {
        vec![
            json!({"role": "system", "content": "SYS"}),
            json!({"id": 41, "role": "user", "content": src}),
            json!({"role": "user", "content": "go"}),
        ]
    };
    let first = run(&c, &Hermes, &view(&src_a), &Opts::default());
    let other = run(&c, &Hermes, &view(&src_b), &Opts::default());
    assert_eq!(other.rows, 1, "the other head is another source");
    let again = run(&c, &Hermes, &view(&src_b), &Opts::default());
    assert_eq!(again.rows, 0);
    assert_eq!(again.messages, other.messages);
    assert_ne!(first.messages, other.messages);
}

// ─── the seeded history generator ────────────────────────────────────────────

const WORDS: &[&str] = &[
    "план",
    "нотатки",
    "ї",
    "й",
    "і\u{0308}",
    "и\u{0306}",
    "👩\u{200d}💻",
    "\u{feff}",
    "\u{2028}",
    "code();",
    "**жирний**",
    "a",
    "Z",
    "42",
    "так-так",
    "«лапки»",
];
const SPACES: &[&str] = &[" ", "  ", "\t", "\n", "\r\n", "\n\n", " \n"];
const BODIES: &[&str] = &[
    "fn main() {\n    println!(\"ї й\");\n}\n",
    "</call>",
    "<call name=\"done\"></call>",
    "<tool_call>",
    "</tool_call>",
    "{\"name\": \"x\", \"arguments\": {}}",
    "\"}",
    "\\\\ back\\slash",
    THINK_OPEN,
    THINK_CLOSE,
    "\r\n",
    "  trailing  ",
    "рядок\n",
];
const TARGETS: &[&str] = &["notes", "kernel", "a|b", "x*", "$parent"];

/// One seeded history: `(role, internal text)` pairs.
struct History(Vec<(&'static str, String)>);

impl Seeded {
    fn prose(&mut self) -> String {
        let mut s = String::new();
        for i in 0..=self.below(6) {
            if i > 0 {
                s.push_str(self.pick(SPACES));
            }
            s.push_str(self.pick(WORDS));
        }
        s
    }

    fn scalar(&mut self) -> String {
        (0..=self.below(3))
            .map(|_| *self.pick(&["значення", "plan.md", "ї", "x_1", "👩\u{200d}💻", "N"]))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn body(&mut self) -> String {
        (0..=self.below(4)).map(|_| *self.pick(BODIES)).collect()
    }

    fn send(&mut self) -> String {
        let mut args = Map::new();
        args.insert("target_id".into(), json!(*self.pick(TARGETS)));
        if self.below(2) == 0 {
            args.insert(
                "reply_to".into(),
                json!(*self.pick(&["self", "null", "notes"])),
            );
        }
        if self.below(2) == 0 {
            args.insert("title".into(), json!(self.scalar()));
        }
        let mut payload = Map::new();
        payload.insert(
            "type".into(),
            json!(*self.pick(&["read", "write", "reflect"])),
        );
        if self.below(2) == 0 {
            payload.insert("path".into(), json!(self.scalar()));
        }
        if self.below(3) == 0 {
            payload.insert("lines".into(), json!(["1", "3"]));
        }
        if self.below(3) == 0 {
            payload.insert(
                "opts".into(),
                json!({"depth": "2", "mode": {"deep": self.scalar()}}),
            );
        }
        if self.below(2) == 0 {
            payload.insert("bytes".into(), json!(self.body()));
        }
        args.insert("payload".into(), Value::Object(payload));
        render_call("send", &args)
    }

    fn control(&mut self) -> String {
        let (name, args) = match self.below(6) {
            0 => ("done", json!({})),
            1 => ("await", json!({})),
            2 => ("inquiry_answer", json!({"answer": self.scalar()})),
            3 => (
                "keep",
                json!({"of": format!("{}", 10 + self.below(90)), "summary": self.scalar()}),
            ),
            4 => ("line_summary", json!({"text": self.scalar()})),
            _ => ("stop", json!({"reason": self.scalar()})),
        };
        render_call(name, args.as_object().expect("an object"))
    }

    fn assistant(&mut self) -> String {
        let mut s = String::new();
        if self.below(3) == 0 {
            s.push_str(THINK_OPEN);
            s.push_str(&self.prose());
            s.push_str(THINK_CLOSE);
            s.push('\n');
        }
        if self.below(2) == 0 {
            s.push_str(&self.prose());
            s.push('\n');
        }
        for _ in 0..self.below(3) {
            s.push_str(&self.send());
            s.push('\n');
        }
        if self.below(2) == 0 {
            s.push_str(&self.control());
        }
        if s.is_empty() {
            s = self.prose();
        }
        s
    }

    fn user(&mut self) -> String {
        if self.below(2) == 0 {
            return self.prose();
        }
        (0..=self.below(2))
            .map(|_| {
                let pid = 10 + self.below(90);
                let body = if self.below(2) == 0 {
                    self.prose()
                } else {
                    format!("Shape: {}", self.send())
                };
                format!("[reply to {pid} · notes read]\n{body}")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn history(&mut self) -> History {
        let n = 2 + self.below(6);
        History(
            (0..n)
                .map(|i| {
                    if i % 2 == 0 {
                        ("user", self.user())
                    } else {
                        ("assistant", self.assistant())
                    }
                })
                .collect(),
        )
    }

    /// Up to three char boundaries of `s`, ascending.
    fn cuts(&mut self, s: &str) -> Vec<usize> {
        let mut at: Vec<usize> = (0..3)
            .map(|_| self.below(s.len() + 1))
            .map(|mut i| {
                while !s.is_char_boundary(i) {
                    i -= 1;
                }
                i
            })
            .collect();
        at.sort_unstable();
        at.dedup();
        at
    }
}

/// `s` cut at `at`.
fn pieces<'a>(s: &'a str, at: &[usize]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut from = 0;
    for &i in at {
        out.push(&s[from..i]);
        from = i;
    }
    out.push(&s[from..]);
    out
}

/// The seeds of D7 and D8.
const SEEDS: std::ops::Range<u64> = 0x0766_0000..0x0766_0000 + 2000;

// D7
/// Every registered adapter, 2 000 seeded histories: each message rendered twice is
/// byte-equal; the reader over a rendered answer gives back its internal text, in one
/// delta and split at seeded offsets. A failing seed is printed.
#[test]
fn property_render_and_round_trip() {
    for seed in SEEDS {
        let mut g = Seeded::new(seed);
        let History(msgs) = g.history();
        for (r, reader) in adapters() {
            for (i, (role, text)) in msgs.iter().enumerate() {
                let once = r.render(text);
                assert_eq!(
                    r.render(text),
                    once,
                    "seed {seed:#x}, {}: message {i} renders twice the same",
                    r.name()
                );
                if *role != "assistant" {
                    continue;
                }
                let whole = read_pieces(reader, &[&once]).unwrap_or_else(|e| {
                    panic!("seed {seed:#x}, {}: message {i}: {e}\n{once}", r.name())
                });
                assert_eq!(&whole, text, "seed {seed:#x}, {}: message {i}", r.name());
                let at = g.cuts(&once);
                let split = read_pieces(reader, &pieces(&once, &at)).unwrap_or_else(|e| {
                    panic!(
                        "seed {seed:#x}, {}: message {i} cut at {at:?}: {e}",
                        r.name()
                    )
                });
                assert_eq!(
                    &split,
                    text,
                    "seed {seed:#x}, {}: message {i} cut at {at:?}",
                    r.name()
                );
            }
        }
    }
}

/// A seeded history as a pass's view: the system message, its messages with ids, an
/// arriving message.
fn as_view(h: &History) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": "SYS"})];
    out.extend(
        h.0.iter()
            .enumerate()
            .map(|(i, (role, text))| json!({"id": i + 1, "role": role, "content": text})),
    );
    out.push(json!({"role": "user", "content": "go"}));
    out
}

// D8
/// The D7 histories through the cache, cold, warm, after an eviction and after an epoch
/// reset: every view is byte-equal to the uncached render.
#[test]
fn property_cache_equals_plain() {
    let other = epoch_of(b"[other rules]", &[&Hermes as &dyn Renderer]);
    for seed in SEEDS {
        let mut g = Seeded::new(seed);
        let m = as_view(&g.history());
        for (r, _) in adapters() {
            let want = plain(r, &m);
            let c = <RenderCache>::new();
            let passes = [
                ("cold", Opts::default()),
                ("warm", Opts::default()),
                ("evicted", Opts::bound(1)),
                ("after the eviction", Opts::default()),
                (
                    "after a reset",
                    Opts {
                        epoch: other,
                        ..Opts::default()
                    },
                ),
            ];
            for (what, o) in passes {
                assert_eq!(
                    run(&c, r, &m, &o).messages,
                    want,
                    "seed {seed:#x}, {}: {what}",
                    r.name()
                );
            }
        }
    }
}

// D10
/// `history.json` assembled and rendered by child processes under another locale, time
/// zone and working directory, and through a cache whose clock stands at 0 and at a far
/// future time: every output byte-equal to the parent's, `ts` heads included; only
/// `kept_endpoint` follows the clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_rows_any_environment() {
    let rendered = d10_output().await;
    if let Some(role) = child_role() {
        zone_took(&role);
        let out = std::env::var(CHILD_OUTPUT).expect("an output file");
        std::fs::write(out, serde_json::to_string(&rendered).unwrap()).expect("written");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let elsewhere = tempfile::TempDir::new().unwrap();
    for (role, envs, cwd) in d10_envs() {
        let file = tmp.path().join(format!("{role}.json"));
        let path = file.to_string_lossy().to_string();
        let mut vars: Vec<(&str, &str)> = envs.to_vec();
        vars.push((CHILD_OUTPUT, path.as_str()));
        run_child(
            "same_rows_any_environment",
            role,
            &vars,
            cwd.then(|| elsewhere.path()),
            None,
        );
        let got: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(got, rendered, "{role}");
    }

    // the clock behind `kept_endpoint`, injected
    let m = history_messages();
    let at_zero = RenderCache::<super::cache::Sha>::with_clock(|| 0.0);
    let far = RenderCache::<super::cache::Sha>::with_clock(|| 1.0e12);
    let o = Opts {
        last_send: Some(1.0),
        ..Opts::default()
    };
    for r in [&Tags as &dyn Renderer, &Hermes] {
        run(&at_zero, r, &m, &o);
        run(&far, r, &m, &o);
        let (a, b) = (run(&at_zero, r, &m, &o), run(&far, r, &m, &o));
        assert_eq!(a.messages, b.messages, "{}", r.name());
        assert_eq!(a.messages, plain(r, &m), "{}", r.name());
        assert_eq!(a.kept, b.kept, "{}", r.name());
        assert_eq!(a.kept_endpoint, a.kept, "{}: the clock says warm", r.name());
        assert_eq!(b.kept_endpoint, 0, "{}: the clock says cold", r.name());
    }
}

/// What D10 compares: `history.json` rendered row by row, and assembled by a chat over
/// its rows and rendered, for D and H.
async fn d10_output() -> Value {
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "d10_rows").await;
    let rows: Vec<Value> = serde_json::from_str(HISTORY).expect("history.json");
    let res = as_agent(
        agent.clone(),
        crate::memory::store_call(&agent, &kernel, json!({"type": "append", "rows": rows})),
    )
    .await
    .expect("store append");
    assert!(res.get("error").is_none(), "{res:?}");
    let state = crate::state::state_for(&agent);
    let (msgs, _) = as_agent(
        agent.clone(),
        crate::assembly::assemble_messages(
            &agent,
            &state,
            "next",
            &kernel,
            "c",
            crate::caching::Caching::ProviderCache,
        ),
    )
    .await
    .expect("assemble");
    crate::state::drop_state(&agent);
    let mut out = Map::new();
    for (r, name) in [(&Tags as &dyn Renderer, "d"), (&Hermes, "h")] {
        out.insert(name.into(), json!(rendered_history(r)));
        let assembled: Vec<Value> = plain(r, &msgs[1..])
            .into_iter()
            .map(|m| m["content"].clone())
            .collect();
        out.insert(format!("assembled_{name}"), Value::Array(assembled));
    }
    Value::Object(out)
}

// D11
/// A restart under another time zone, through the assembly: rows appended by a child
/// under `TZ=Europe/Kyiv`, the store opened by a child under `TZ=America/Los_Angeles`
/// and by one under `TZ=UTC`. Every head is the head the writer sent, so the sources
/// equal the pre-stop ones; the first pass is cold only because the cache is new; a row
/// appended after the restart carries the new zone's offset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_zone_same_heads() {
    if let Some(role) = child_role() {
        d11_child(&role).await;
        return;
    }
    for tz in ["Europe/Kyiv", "America/Los_Angeles"] {
        if !zone_known(tz) {
            eprintln!("D11: the zone {tz} is not on this system: skipped");
            return;
        }
    }
    let tmp = tempfile::TempDir::new().unwrap();
    run_child(
        "other_zone_same_heads",
        "d11_write",
        &[("TZ", "Europe/Kyiv")],
        None,
        Some(tmp.path()),
    );
    let writer: Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join("writer.json")).unwrap())
            .unwrap();
    for r in writer["rows"].as_array().unwrap() {
        if r["attributes"].get("ts").is_some() {
            let off = r["attributes"][UTC_OFFSET].as_i64();
            assert!(matches!(off, Some(7200 | 10_800)), "a Kyiv offset: {r}");
        }
    }
    let pre: Vec<Value> = writer["request"].as_array().cloned().unwrap();
    for (role, tz) in [
        ("d11_read_la", "America/Los_Angeles"),
        ("d11_read_utc", "UTC"),
    ] {
        let copy = tempfile::TempDir::new().unwrap();
        copy_dir(tmp.path(), copy.path());
        run_child(
            "other_zone_same_heads",
            role,
            &[("TZ", tz)],
            None,
            Some(copy.path()),
        );
        let reader: Value = serde_json::from_str(
            &std::fs::read_to_string(copy.path().join("reader.json")).unwrap(),
        )
        .unwrap();
        let now: Vec<Value> = reader["request"].as_array().cloned().unwrap();
        let k = pre.len() - 1;
        assert_eq!(now[1..k], pre[1..k], "{role}: the heads the writer sent");
        assert!(
            reader["events"]
                .as_array()
                .unwrap()
                .contains(&json!("render_cold")),
            "{role}: {}",
            reader["events"]
        );
    }
}

/// Every file under `from` copied to the same place under `to`.
fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for e in std::fs::read_dir(from).expect("read_dir") {
        let e = e.expect("entry");
        let target = to.join(e.file_name());
        if e.file_type().expect("type").is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).expect("copy");
        }
    }
}

/// D11's children: the writer appends two turns on a file store; a reader opens the
/// store again, makes one pass and appends its rows.
async fn d11_child(role: &str) {
    use super::tests_trace::{answered, names, store, traces_of, Scripted, PLAIN};
    let dir = std::path::PathBuf::from(std::env::var(CHILD_INPUT).expect("a folder"));
    let fields = json!({"model": PLAIN});
    if role == "d11_write" {
        let s = super::tests_restart::site(&dir, "d11_chat", fields, None).await;
        let p = Scripted::new(PLAIN, vec![]);
        answered(&s.kernel, &s.agent, &p, "d11_0").await;
        answered(&s.kernel, &s.agent, &p, "d11_1").await;
        let rows = store(&s.kernel, &s.agent).await;
        for r in &rows {
            if let Some(ts) = r["attributes"].get("ts") {
                assert_eq!(
                    r["attributes"].get(UTC_OFFSET).cloned(),
                    offset_at(ts),
                    "{r}"
                );
            }
        }
        let request = p.asked().last().cloned().unwrap();
        let ids = s.stop();
        std::fs::write(
            dir.join("writer.json"),
            serde_json::to_string(&json!({"request": request, "rows": rows, "ids": ids})).unwrap(),
        )
        .expect("written");
        return;
    }
    let writer: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("writer.json")).unwrap()).unwrap();
    let s = super::tests_restart::site(&dir, "d11_chat", fields, writer["ids"].as_i64()).await;
    let p = Scripted::new(PLAIN, vec![]);
    answered(&s.kernel, &s.agent, &p, "d11_read").await;
    let events = names(&traces_of(&s.kernel, &s.agent, "d11_read").await);
    // the row appended after the restart: this zone's offset
    let rows = store(&s.kernel, &s.agent).await;
    let newest = rows
        .iter()
        .rev()
        .find(|r| r["role"] == json!("user"))
        .expect("the new incoming row");
    let ts = newest["attributes"]["ts"].clone();
    assert_eq!(
        newest["attributes"].get(UTC_OFFSET).cloned(),
        offset_at(&ts),
        "{newest}"
    );
    let request = p.asked().last().cloned().unwrap();
    s.stop();
    std::fs::write(
        dir.join("reader.json"),
        serde_json::to_string(&json!({"request": request, "events": events})).unwrap(),
    )
    .expect("written");
}

/// D12's rows: one minute before the change of 2026-10-25 01:00 UTC, and at it.
const DST: [(i64, i64, &str); 2] = [
    (1_792_889_940, 10_800, "[2026-10-25 03:59 +03:00]\n"),
    (1_792_890_000, 7_200, "[2026-10-25 03:00 +02:00]\n"),
];

// D12
/// A DST change: under `TZ=Europe/Kyiv`, rows appended one minute before and at the
/// change keep offsets 10800 and 7200 and the heads of their own instant; both heads are
/// unchanged when the store is read after the change, and under another zone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dst_rows_keep_own_offset() {
    if let Some(role) = child_role() {
        let file = std::path::PathBuf::from(std::env::var(CHILD_INPUT).expect("a file"));
        if role == "d12_kyiv" {
            let tmp = tempfile::TempDir::new().unwrap();
            let (kernel, agent) = mk_kernel(&tmp, "d12_dst").await;
            let rows: Vec<Value> = DST
                .iter()
                .enumerate()
                .map(|(i, (ts, ..))| {
                    let role = if i == 0 { "user" } else { "assistant" };
                    json!({"role": role, "content": format!("dst-{i}"), "attributes": {"ts": ts}})
                })
                .collect();
            let ids = save(&kernel, &agent, &rows).await;
            let stored = store(&kernel, &agent).await;
            let back: Vec<Value> = ids.iter().map(|id| by_id(&stored, *id)).collect();
            for (row, (_, off, h)) in back.iter().zip(DST) {
                assert_eq!(row["attributes"][UTC_OFFSET], json!(off), "{row}");
                assert_eq!(head(row), h, "{row}");
            }
            std::fs::write(file, serde_json::to_string(&back).unwrap()).expect("written");
        } else {
            zone_took(&role);
            let back: Vec<Value> =
                serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            for (row, (.., h)) in back.iter().zip(DST) {
                assert_eq!(head(row), h, "{role}: {row}");
            }
        }
        return;
    }
    if !zone_known("Europe/Kyiv") {
        eprintln!("D12: the zone Europe/Kyiv is not on this system: skipped");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let file = tmp.path().join("d12.json");
    run_child(
        "dst_rows_keep_own_offset",
        "d12_kyiv",
        &[("TZ", "Europe/Kyiv")],
        None,
        Some(&file),
    );
    // read after the change, in this process's zone
    let back: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    for (row, (_, off, h)) in back.iter().zip(DST) {
        assert_eq!(row["attributes"][UTC_OFFSET], json!(off));
        assert_eq!(head(row), h);
    }
    // and under another zone
    if zone_known("America/Los_Angeles") {
        run_child(
            "dst_rows_keep_own_offset",
            "tz_los_angeles",
            &[("TZ", "America/Los_Angeles")],
            None,
            Some(&file),
        );
    }
}
