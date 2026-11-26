//! Restart, shutdown and crash (#766, W11): RS1-RS18 of spec 2.13.
//!
//! A RESTART is a new kernel over the same folder and `sqlite_store` file, with no
//! `BackendState` for the chat, so with a new, empty render cache: [`Site::stop`] drops
//! the chat's state and head and closes the file, and [`site`] opens it all again. A
//! started kernel persists the id bound it reserved and the next one mints above it;
//! a test kernel is never started, so the bound is handed to the next kernel
//! ([`site`]'s `ids_from`), as `Kernel.load` does with the persisted one.
//!
//! A CRASH is the same with nothing run on the way out: the pass's future is polled to
//! its crash point and then forgotten ([`Site::crash`]), so no destructor runs (its pin
//! guard included) and it is never polled again; only the chat's state slot is
//! released, so the next kernel's chat gets a new one. RS7 kills a real child process.
//!
//! Each case runs for `default` and `hermes` unless it says otherwise. "Pre-stop bytes"
//! are the rendered messages of the last finished pass before the stop: the request it
//! sent, the system message and the arriving message set aside.
//!
//! WHAT A FOLD WRITES (RS5, owner 2026-11-25): a fold's rows are made unwritten
//! (`strategies/fold.rs` `fold_row`) and go in ONE store append with the view row that
//! shows them, folds first (`memory::save_view`), so a crash leaves both or neither:
//! - `context_fold` (`retrieval.rs`): the summary (a task pass, or the given text), then
//!   one append of the fold row and the view row.
//! - the fold strategy (`projection::project_context`): per folded chain its summary, in
//!   turn, then one append of every fold row made and the projection's view row.
//! - the strip strategy: the view row alone (stubs are a resolution of the view).
//!
//! So no point between two writes falls inside a fold: a crash stops before a fold's
//! append or after it.
//!
//! A FOLD'S IDS (owner 2026-11-25, fold id order): a fold row is made under a provisional
//! id (`folds::provisional_id`); `memory::save_view` mints its ids in that one append and
//! rewrites the provisional ones (`folds::remap_ids`), so a row appended while a
//! summarizer ran has a lower id than the folds and keeps its place after a restart.
//!
//! A summary that stopped at its bound (`stop_reason` `max_tokens`) is refused before
//! its row is written (`strategies/mod.rs` `summarize`).
//!
//! WHAT THE ASSEMBLY DOES WITH AN ORPHAN, read for RS6 (`assembly.rs`
//! `listed_entries`, `render_history`): the newest view row's ids are shown at their
//! resolution, then every row stored after the newest of them, as stored; with no view
//! row, the whole store as stored. So an incoming row with no assistant row is shown as
//! a user message at the end; a fold row with no view row (no fold writes one since
//! RS5: built by hand) is shown as a stored row after the rows the last view row lists
//! (its members still shown whole); a view row that names a row the store does not
//! hold is a stale array, and the whole store is shown as stored.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

use super::cache::{epoch_of, Hash, RenderCache};
use super::default::Tags;
use super::hermes::{self, Hermes};
use super::rules::{load, shipped, type_of, RULES_JSON};
use super::tests_cache::{conv, decl_of, finish, open, plain, refs, rows_n, run, Count, Opts};
use super::tests_determinism::{recorded, rendered_history, rules_digest};
use super::tests_trace::{
    answered, assembled, chat_on, generate, names, operator, restart, set_model, store,
    store_agent, traces_of, verb, Scripted, Step, DONE, FOREIGN, PLAIN, QWEN,
};
use super::Renderer;
use crate::provider::ProviderReport;

/// The two types and a model id of each.
const KINDS: [(&str, &str); 2] = [("default", PLAIN), ("hermes", QWEN)];
/// A hermes model id of Qwen 2.5.
const QWEN25: &str = "qwen2.5-7b-instruct";
/// The hermes `done` call, as a hermes model writes it.
const H_DONE: &str = "<tool_call>\n{\"name\": \"done\", \"arguments\": {}}\n</tool_call>";
/// The variable that tells a child run of this binary which side of a test it is.
const CHILD_ROLE: &str = "FANTASTIC_RS_CHILD_ROLE";
/// The folder a child works in.
const CHILD_DIR: &str = "FANTASTIC_RS_CHILD_DIR";
/// The id bound a child's kernel starts above.
const CHILD_IDS: &str = "FANTASTIC_RS_CHILD_IDS";
/// A bound small enough to evict: 64 KiB.
const SMALL: usize = 64 * 1024;

// ─── the site: a kernel over a folder, a chat on a file store ────────────────

/// A kernel over a folder, and the chat on it.
pub(super) struct Site {
    pub(super) kernel: Arc<Kernel>,
    pub(super) agent: AgentId,
}

/// The chat `chat` on a `sqlite_store` file in `dir`, under a new kernel whose ids start
/// above `ids_from`. `fields` go on its record; a `can` among them adds lines; a
/// `provider_id` makes the provider agent `prov` on the echo bundle, whose `models`
/// answers [`QWEN`] as its default, and gives the chat a line to ask it.
pub(super) async fn site(dir: &Path, chat: &str, fields: Value, ids_from: Option<i64>) -> Site {
    let kernel = crate::tests::base_kernel_at(dir, |k| {
        k.bundles.register(
            fantastic_sqlite_store::HANDLER_MODULE,
            fantastic_sqlite_store::SqliteStoreBundle,
        );
        k.bundles
            .register("echo_provider.tools", crate::tests::EchoProvider);
    })
    .await;
    kernel.rows.restore_counter(ids_from);
    let db = format!("{chat}_db");
    store_agent(
        &kernel,
        &db,
        fantastic_sqlite_store::HANDLER_MODULE,
        json!({"path": format!("{chat}.db")}),
    )
    .await;
    let mut can = Map::new();
    can.insert(db.clone(), json!({"call": ["*"]}));
    can.insert("cli".into(), json!({"call": ["*"]}));
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": "mock_backend.tools",
        "id": chat,
        "context_window": 32768,
        "memory_store_id": db,
    });
    for (k, v) in fields.as_object().cloned().unwrap_or_default() {
        if k == "can" {
            can.extend(v.as_object().cloned().unwrap_or_default());
        } else {
            create[k] = v;
        }
    }
    if create.get("provider_id").is_some() {
        crate::tests::make_provider(&kernel, "prov", json!({"models": [QWEN]}), Some(QWEN)).await;
        can.insert("prov".into(), json!({"call": ["*"]}));
    }
    create["can"] = Value::Object(can);
    let r = operator(&kernel, "kernel_state", create).await;
    assert!(r.get("error").is_none(), "create {chat}: {r}");
    Site {
        kernel,
        agent: AgentId::from(chat),
    }
}

impl Site {
    /// A clean shutdown: the chat's state and head dropped, the file closed, the kernel
    /// gone. The id bound the next kernel starts above.
    pub(super) fn stop(self) -> Option<i64> {
        restart(&self.kernel, &self.agent);
        assert!(fantastic_sqlite_store::close_all(&self.kernel).is_empty());
        Some(self.kernel.rows.id_reserved())
    }

    /// A crash mid-pass: `pass` is forgotten, so nothing of it runs again, no destructor
    /// included; only the chat's state slot is released. The id bound.
    fn crash<F>(self, pass: F) -> Option<i64> {
        std::mem::forget(pass);
        crate::state::drop_state(&self.agent);
        Some(self.kernel.rows.id_reserved())
    }
}

/// One generation of the site's chat as `client`, as a future the test polls itself.
fn pass_future(site: &Site, p: &Arc<Scripted>, client: &str) -> Pin<Box<dyn Future<Output = ()>>> {
    let (k, a, p, c) = (
        Arc::clone(&site.kernel),
        site.agent.clone(),
        Arc::clone(p),
        client.to_string(),
    );
    Box::pin(async move {
        let _ = generate(&k, &a, &p, &c).await;
    })
}

/// Poll `fut` until `reached` holds (at most 5 s), then a little longer so what the
/// endpoint sent is read; panics when the pass ends first.
async fn drive<F: Future + Unpin>(fut: &mut F, reached: impl Fn() -> bool, what: &str) {
    for _ in 0..500 {
        if reached() {
            break;
        }
        tokio::select! {
            _ = &mut *fut => panic!("{what}: the pass ended before its crash point"),
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }
    assert!(reached(), "{what}: the crash point was never reached");
    tokio::select! {
        _ = &mut *fut => panic!("{what}: the pass ended at its crash point"),
        () = tokio::time::sleep(Duration::from_millis(150)) => {}
    }
}

/// The last request `p` was asked.
fn last(p: &Scripted) -> Vec<Value> {
    p.asked().last().cloned().expect("a request")
}

/// `now`'s messages for the rows `pre` showed are byte-equal to `pre`'s: the system
/// message and `pre`'s arriving message set aside.
fn same_rows(pre: &[Value], now: &[Value], what: &str) {
    let k = pre.len() - 1;
    assert!(now.len() >= k, "{what}: {} messages, {k} before", now.len());
    // the system block too: its date is the thread's first row's, never the clock's
    assert_eq!(now[..k], pre[..k], "{what}: the pre-stop bytes");
}

/// The kept chain of `kind` in the chat's cache.
fn chain(agent: &AgentId, kind: &str) -> Vec<Hash> {
    crate::state::state_for(agent)
        .render_cache
        .chain(kind)
        .unwrap_or_else(|| panic!("no kept chain of {kind}"))
}

/// The first `render_*` trace row of `client`'s turn.
async fn render_row(site: &Site, client: &str) -> Value {
    let rows = traces_of(&site.kernel, &site.agent, client).await;
    rows.iter()
        .find(|r| {
            r["event"]
                .as_str()
                .is_some_and(|e| e.starts_with("render_"))
        })
        .cloned()
        .unwrap_or_else(|| panic!("{client}: no render row in {rows:?}"))
}

/// The messages of `msgs` that carry a row id.
/// The turn requests `p` was asked: the ones with a system message. The others are task
/// passes: after a restart the first turn runs the recall the boot owes (`recall_due`,
/// `verbs.rs` `boot`) through the same provider.
fn turn_requests(p: &Scripted) -> Vec<Vec<Value>> {
    p.asked()
        .into_iter()
        .filter(|r| r.first().is_some_and(|m| m["role"] == json!("system")))
        .collect()
}

/// The conversation rows stored after `before` other than the turn's own two: the row
/// of the recall the boot owes, shown in the turn's view.
async fn owed_rows(kernel: &Arc<Kernel>, agent: &AgentId, before: &[Value]) -> usize {
    let after = max_id(before);
    let new = store(kernel, agent)
        .await
        .into_iter()
        .filter(|r| r.get("role").is_some_and(Value::is_string))
        .filter(|r| r["id"].as_i64().is_some_and(|id| id > after))
        .count();
    new.saturating_sub(2)
}

fn with_ids(msgs: &[Value]) -> usize {
    msgs.iter().filter(|m| m.get("id").is_some()).count()
}

/// A fresh state's cache holds nothing: `(entries, chains, pins)`.
fn empty_cache(agent: &AgentId) -> (usize, usize, usize) {
    let s = crate::state::state_for(agent).render_cache.stats();
    (s.entries, s.chains, s.pins)
}

/// `rows` saved through the chat's append; their ids.
async fn save(site: &Site, rows: &[Value]) -> Vec<i64> {
    as_agent(
        site.agent.clone(),
        crate::memory::save_history(&site.agent, &site.kernel, rows),
    )
    .await
    .expect("rows saved")
}

/// `rows` appended to `agent`'s store as they are (their ids and attributes kept).
async fn append_raw(kernel: &Arc<Kernel>, agent: &AgentId, rows: &[Value]) {
    if rows.is_empty() {
        return;
    }
    let res = as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "append", "rows": rows})),
    )
    .await
    .expect("store append");
    assert!(res.get("error").is_none(), "{res:?}");
}

/// Set fields on a chat's record.
async fn set(kernel: &Arc<Kernel>, agent: &AgentId, fields: &Value) {
    let mut p = json!({"type": "update_agent", "id": agent.as_str()});
    for (k, v) in fields.as_object().cloned().unwrap_or_default() {
        p[k] = v;
    }
    let r = operator(kernel, "kernel_state", p).await;
    assert!(r.get("error").is_none(), "update_agent refused: {r}");
}

/// The hex of each link.
fn hex(links: &[Hash]) -> Vec<String> {
    links
        .iter()
        .map(|h| h.iter().map(|b| format!("{b:02x}")).collect())
        .collect()
}

/// Every file under `from` copied to the same place under `to`.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for e in std::fs::read_dir(from).expect("read_dir") {
        let e = e.expect("entry");
        let target = to.join(e.file_name());
        if e.file_type().expect("type").is_dir() {
            copy_tree(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).expect("copy");
        }
    }
}

// ─── child processes ─────────────────────────────────────────────────────────

fn child_role() -> Option<String> {
    std::env::var(CHILD_ROLE).ok()
}

fn child_dir() -> PathBuf {
    PathBuf::from(std::env::var(CHILD_DIR).expect("a child's folder"))
}

fn child_ids() -> Option<i64> {
    std::env::var(CHILD_IDS).ok().and_then(|v| v.parse().ok())
}

/// The command that runs `test` of this module again as a child, as `role`.
fn child(
    test: &str,
    role: &str,
    dir: &Path,
    ids: Option<i64>,
    envs: &[(&str, &str)],
) -> std::process::Command {
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
    .env(CHILD_ROLE, role)
    .env(CHILD_DIR, dir);
    if let Some(n) = ids {
        c.env(CHILD_IDS, n.to_string());
    }
    for (k, v) in envs {
        c.env(k, v);
    }
    c
}

/// Run a child to its end; panics unless it ran its test and the test passed.
fn run_child(mut c: std::process::Command, what: &str) {
    let out = c.output().expect("the child runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "child {what}:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write_json(path: &Path, v: &Value) {
    std::fs::write(path, serde_json::to_string(v).expect("json")).expect("write");
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json")
}

// ─── RS1-RS4 ─────────────────────────────────────────────────────────────────

// RS1
/// A clean shutdown, then a restart and two sends: pass 1 is cold over every row, its
/// messages for the rows stored before the stop are the pre-stop bytes, `kept == 0` and
/// the links equal the pre-stop links; pass 2 is warm with only the new rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_restart_cold_then_warm() {
    for (kind, model) in KINDS {
        let tmp = TempDir::new().unwrap();
        let chat = format!("rs1_{kind}");
        let fields = json!({"model": model});
        let s = site(tmp.path(), &chat, fields.clone(), None).await;
        let p = Scripted::new(model, vec![]);
        for i in 0..3 {
            answered(&s.kernel, &s.agent, &p, &format!("{chat}_{i}")).await;
        }
        let pre = last(&p);
        let pre_links = chain(&s.agent, kind);
        let ids = s.stop();

        let s = site(tmp.path(), &chat, fields, ids).await;
        assert_eq!(empty_cache(&s.agent), (0, 0, 0), "{kind}");
        let p = Scripted::new(model, vec![]);
        answered(&s.kernel, &s.agent, &p, &format!("{chat}_after")).await;
        // every stored row the turn showed: the system message and the arriving one aside
        let n = turn_requests(&p).last().expect("the turn's request").len() - 2;
        let cold = render_row(&s, &format!("{chat}_after")).await;
        assert_eq!(cold["event"], json!("render_cold"), "{kind}: {cold}");
        // every type maps and keeps its rows
        assert_eq!(cold["rows"], json!(n), "{kind}: {cold}");
        assert_eq!(cold["kept"], json!(0), "{kind}: {cold}");
        let now = last(&p);
        same_rows(&pre, &now, kind);
        let k = pre.len() - 1;
        assert_eq!(chain(&s.agent, kind)[..k], pre_links[..k], "{kind}: links");

        answered(&s.kernel, &s.agent, &p, &format!("{chat}_after2")).await;
        let warm = render_row(&s, &format!("{chat}_after2")).await;
        assert_eq!(warm["event"], json!("render_warm"), "{kind}: {warm}");
        assert_eq!(warm["rows"], json!(2), "{kind}: only the new rows");
        s.stop();
    }
}

/// A crash at the point `steps` leave the pass in (the last one holds it), for `kind`:
/// nothing of the send is in the store, the new cache is empty before its first pass,
/// and the next send starts at attempt 1 with the pre-stop bytes and none of `dead`.
async fn crash_at(kind: &str, model: &str, steps: Vec<Step>, what: &str, dead: &[&str]) {
    let tmp = TempDir::new().unwrap();
    let chat = format!("rs_{what}_{kind}");
    let fields = json!({"model": model});
    let s = site(tmp.path(), &chat, fields.clone(), None).await;
    let p = Scripted::new(model, vec![]);
    for i in 0..2 {
        answered(&s.kernel, &s.agent, &p, &format!("{chat}_{i}")).await;
    }
    let pre = last(&p);
    let before = store(&s.kernel, &s.agent).await;
    let (asked, n) = (p.asked().len(), steps.len());
    p.push(steps);
    let mut fut = pass_future(&s, &p, &format!("{chat}_dead"));
    let probe = Arc::clone(&p);
    drive(&mut fut, move || probe.asked().len() >= asked + n, what).await;
    assert_eq!(
        store(&s.kernel, &s.agent).await,
        before,
        "{what} {kind}: the store is as before the send"
    );
    let ids = s.crash(fut);

    let s = site(tmp.path(), &chat, fields, ids).await;
    assert_eq!(
        empty_cache(&s.agent),
        (0, 0, 0),
        "{what} {kind}: no entry, chain or pin before the first pass"
    );
    assert_eq!(store(&s.kernel, &s.agent).await, before, "{what} {kind}");
    let p = Scripted::new(model, vec![]);
    answered(&s.kernel, &s.agent, &p, &format!("{chat}_after")).await;
    let asked = turn_requests(&p);
    assert_eq!(asked.len(), 1, "{what} {kind}: one request");
    let now = &asked[0];
    let owed = owed_rows(&s.kernel, &s.agent, &before).await;
    assert_eq!(
        now.len(),
        pre.len() + 2 + owed,
        "{what} {kind}: no re-ask pair"
    );
    same_rows(&pre, now, &format!("{what} {kind}"));
    for m in now {
        let c = m["content"].as_str().unwrap_or("");
        for d in dead {
            assert!(
                !c.contains(d),
                "{what} {kind}: {d:?} is in the request: {c}"
            );
        }
    }
    s.stop();
}

/// The answer the reader of `kind` fails an attempt at.
fn bad(kind: &str) -> &'static str {
    if kind == hermes::NAME {
        DONE
    } else {
        FOREIGN
    }
}

// RS2
/// A crash after the typed view was built, while the request is out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_mid_pass() {
    for (kind, model) in KINDS {
        crash_at(kind, model, vec![Step::stalls()], "mid_pass", &[]).await;
    }
}

// RS3
/// A crash mid-stream: tokens arrived, no reply yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_mid_stream() {
    for (kind, model) in KINDS {
        crash_at(
            kind,
            model,
            vec![Step::hangs("half an answer, never ended")],
            "mid_stream",
            &["half an answer"],
        )
        .await;
    }
}

// RS4
/// A crash mid-retry at attempt m of n, m = 2 and 3: while the re-ask streams, and
/// between a `Bad` and the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_mid_retry() {
    for (kind, model) in KINDS {
        let b = bad(kind);
        // the failed attempt's call form, which no stored row of the chat holds
        let form = if kind == hermes::NAME {
            "<call name="
        } else {
            "<invoke name="
        };
        let dead = ["streaming the re-ask", form];
        for m in [2, 3] {
            let mut streams: Vec<Step> = (1..m).map(|_| Step::says(b)).collect();
            streams.push(Step::hangs("streaming the re-ask"));
            crash_at(kind, model, streams, &format!("retry{m}_stream"), &dead).await;
            let mut asks: Vec<Step> = (1..m).map(|_| Step::says(b)).collect();
            asks.push(Step::stalls());
            crash_at(kind, model, asks, &format!("retry{m}_request"), &dead).await;
        }
    }
}

// ─── RS5, RS6 ────────────────────────────────────────────────────────────────

/// The rows a fold wrote: those after `after`, of kind `fold` or `view`, in id order.
fn fold_writes(rows: &[Value], after: i64) -> Vec<Value> {
    rows.iter()
        .filter(|r| r["id"].as_i64().is_some_and(|id| id > after))
        .filter(|r| matches!(r["kind"].as_str(), Some("fold" | "view")))
        .cloned()
        .collect()
}

/// The highest row id of `rows`.
fn max_id(rows: &[Value]) -> i64 {
    rows.iter()
        .filter_map(|r| r["id"].as_i64())
        .max()
        .unwrap_or(0)
}

/// What a new chat `name` with `fields` shows over `rows` (appended as they are): the
/// next view, the system message set aside.
async fn replayed(kernel: &Arc<Kernel>, name: &str, fields: &Value, rows: &[Value]) -> Vec<Value> {
    let a = chat_on(
        kernel,
        name,
        fantastic_ram_store::test_support::MEMORY,
        None,
    )
    .await;
    if fields.as_object().is_some_and(|f| !f.is_empty()) {
        set(kernel, &a, fields).await;
    }
    append_raw(kernel, &a, rows).await;
    let msgs = assembled(kernel, &a).await;
    crate::state::drop_state(&a);
    unowned(&msgs[1..])
}

/// `msgs` without the store's `agent` stamp: the owner a row was read under, which a
/// replay under another chat changes and which no request carries.
fn unowned(msgs: &[Value]) -> Vec<Value> {
    msgs.iter()
        .map(|m| {
            let mut m = m.clone();
            if let Some(o) = m.as_object_mut() {
                o.remove("agent");
            }
            m
        })
        .collect()
}

/// `writes` as the store appends that wrote them: a fold's rows go in one append with
/// the view row after them (`memory::save_view`), so each append ends at a view row.
/// A fold row after the last view row is a fold written without its view: refused.
fn appends(tag: &str, writes: &[Value]) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for w in writes {
        cur.push(w.clone());
        if w["kind"] == json!("view") {
            out.push(std::mem::take(&mut cur));
        }
    }
    assert!(
        cur.is_empty(),
        "{tag}: fold rows written without their view row: {cur:?}"
    );
    out
}

/// RS5's check over one fold: every store reached by stopping between two of its
/// writes (store appends, [`appends`]) shows either the unfolded view or the folded
/// one, and each is the view the chat itself showed (or would show) with no crash.
async fn every_point(
    kernel: &Arc<Kernel>,
    tag: &str,
    fields: &Value,
    before: &[Value],
    writes: &[Value],
    unfolded: Option<&[Value]>,
    folded: Option<&[Value]>,
) {
    let writes = appends(tag, writes);
    let base = replayed(kernel, &format!("{tag}_r0"), fields, before).await;
    if let Some(u) = unfolded {
        assert_eq!(base, u, "{tag}: the unfolded state, replayed");
    }
    let mut all = before.to_vec();
    all.extend(writes.iter().flatten().cloned());
    let done = replayed(kernel, &format!("{tag}_rall"), fields, &all).await;
    if let Some(f) = folded {
        assert_eq!(done, f, "{tag}: the folded state, replayed");
    }
    for i in 1..writes.len() {
        let mut part = before.to_vec();
        part.extend(writes[..i].iter().flatten().cloned());
        let shown = replayed(kernel, &format!("{tag}_r{i}"), fields, &part).await;
        let kinds: Vec<&str> = writes[..i]
            .iter()
            .flatten()
            .map(|w| w["kind"].as_str().unwrap_or(""))
            .collect();
        assert!(
            shown == base || shown == done,
            "{tag}: stopped after the writes {kinds:?}, the view is a mix of the two \
             states:\n{shown:#?}"
        );
    }
}

/// A closed chain of `tag`: a task, a call, a large reply.
fn chain_rows(kernel: &Kernel, tag: &str) -> Vec<Value> {
    let pid = kernel.mint_id();
    let words: Vec<String> = (0..500).map(|i| format!("{tag}{i}")).collect();
    vec![
        json!({"role": "user", "content": format!("task {tag}")}),
        json!({"role": "assistant", "content": format!(
            "{tag} <call name=\"send\"><routing><target_id>notes</target_id></routing>\
             <payload><type>read</type></payload></call>")}),
        json!({"role": "user", "content": format!(
            "[reply to {pid} · notes read · \"{tag}\"]\n{}", words.join(" "))}),
    ]
}

// RS5
/// A crash mid-fold, at every point between two store writes of a fold, for
/// `context_fold` (a given summary and the summarizer's), the fold strategy (the
/// autofold summary) and the strip strategy: the restarted view is the unfolded state or
/// the folded one, never a mix; a cut-off summary is not kept as a whole one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_mid_fold() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _) = crate::tests::mk_kernel(&tmp, "rs5_base").await;
    let mem = fantastic_ram_store::test_support::MEMORY;

    // context_fold, the summary given, and the summarizer's
    for (tag, summary) in [("rs5_given", Some("FOLD-RS5")), ("rs5_summarized", None)] {
        let a = chat_on(&kernel, tag, mem, None).await;
        let p = Scripted::new(PLAIN, vec![]);
        for i in 0..3 {
            answered(&kernel, &a, &p, &format!("{tag}_{i}")).await;
        }
        let before = store(&kernel, &a).await;
        let unfolded = unowned(&assembled(&kernel, &a).await[1..]);
        let turns: Vec<i64> = before
            .iter()
            .filter(|r| r.get("role").is_some() && r.get("kind").is_none())
            .filter_map(|r| r["id"].as_i64())
            .collect();
        let mut fold = json!({"type": "context_fold", "from": turns[2], "to": turns[3]});
        match summary {
            Some(s) => fold["summary"] = json!(s),
            None => {
                crate::tests::script(
                    &a,
                    vec![crate::tests::pass("the work, folded\n\nwhat was read.")],
                    Duration::ZERO,
                );
            }
        }
        let out = verb(&kernel, &a, fold).await;
        assert!(out["folded"].is_i64(), "{tag}: context_fold refused: {out}");
        let after = store(&kernel, &a).await;
        let writes = fold_writes(&after, max_id(&before));
        let kinds: Vec<&str> = writes
            .iter()
            .map(|w| w["kind"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(kinds, ["fold", "view"], "{tag}: the writes of a fold");
        let folded = unowned(&assembled(&kernel, &a).await[1..]);
        every_point(
            &kernel,
            tag,
            &json!({}),
            &before,
            &writes,
            Some(unfolded.as_slice()),
            Some(folded.as_slice()),
        )
        .await;
        crate::state::drop_state(&a);
    }

    // the fold strategy and the strip strategy, at a projection
    for (tag, strategy) in [("rs5_autofold", "fold"), ("rs5_strip", "strip")] {
        let fields =
            json!({"context_strategy": strategy, "context_window": 4000, "output_reserve": 0});
        let a = chat_on(&kernel, tag, mem, None).await;
        set(&kernel, &a, &fields).await;
        let mut rows = vec![json!({"role": "user", "content": "GOAL"})];
        for t in ["a", "b", "c", "d", "e", "f"] {
            rows.extend(chain_rows(&kernel, &format!("{tag}_{t}")));
        }
        as_agent(a.clone(), crate::memory::save_history(&a, &kernel, &rows))
            .await
            .expect("rows saved");
        let before = store(&kernel, &a).await;
        let unfolded = unowned(&assembled(&kernel, &a).await[1..]);
        let p = Scripted::new(PLAIN, vec![]);
        let _ = generate(&kernel, &a, &p, &format!("{tag}_turn")).await;
        let writes = fold_writes(&store(&kernel, &a).await, max_id(&before));
        assert!(!writes.is_empty(), "{tag}: the projection wrote nothing");
        if strategy == "fold" {
            assert!(
                writes.iter().any(|w| w["kind"] == json!("fold")),
                "{tag}: the projection folded nothing: {writes:?}"
            );
        } else {
            assert!(
                writes.iter().all(|w| w["kind"] == json!("view")),
                "{writes:?}"
            );
        }
        // the folded state with no crash: the store's own fold writes, its turn rows
        // set aside (they come after the view row)
        every_point(
            &kernel,
            tag,
            &fields,
            &before,
            &writes,
            Some(unfolded.as_slice()),
            None,
        )
        .await;
        crate::state::drop_state(&a);
    }

    // a summary cut off at its bound is not kept as a whole one
    let tag = "rs5_cut";
    let fields = json!({"context_strategy": "fold", "context_window": 4000, "output_reserve": 0});
    let a = chat_on(&kernel, tag, mem, None).await;
    set(&kernel, &a, &fields).await;
    let mut rows = vec![json!({"role": "user", "content": "GOAL"})];
    for t in ["a", "b", "c", "d", "e", "f"] {
        rows.extend(chain_rows(&kernel, &format!("{tag}_{t}")));
    }
    as_agent(a.clone(), crate::memory::save_history(&a, &kernel, &rows))
        .await
        .expect("rows saved");
    let cut = ProviderReport {
        stop_reason: Some("max_tokens".to_string()),
        ..ProviderReport::default()
    };
    let p = Scripted::new(
        PLAIN,
        (0..40)
            .map(|_| Step::says("CUT-OFF SUMMARY").reports(cut.clone()))
            .collect(),
    );
    let _ = generate(&kernel, &a, &p, &format!("{tag}_turn")).await;
    for r in store(&kernel, &a).await {
        if r["kind"] == json!("fold") {
            let c = r["content"].as_str().unwrap_or("");
            assert!(
                !c.contains("CUT-OFF SUMMARY"),
                "a cut-off summary was kept: {r}"
            );
        }
    }
    crate::state::drop_state(&a);
}

// RS6
/// Half-written stores built by hand: an incoming row with no assistant row; a fold row
/// with no view row; a view row naming a fold row that is absent. Every complete row
/// renders as before, the pass does not fail, and the orphan shows as the assembly
/// places it (this module's head). Since RS5 a fold writes its fold row and its view
/// row in one append, so a crash never leaves the fold orphan: a real `context_fold`
/// here writes the two together, and a store cut before that append is the store
/// as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_rows_recover() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _) = crate::tests::mk_kernel(&tmp, "rs6_base").await;
    let mem = fantastic_ram_store::test_support::MEMORY;
    let a = chat_on(&kernel, "rs6_whole", mem, None).await;
    let p = Scripted::new(PLAIN, vec![]);
    for i in 0..2 {
        answered(&kernel, &a, &p, &format!("rs6_{i}")).await;
    }
    let base = store(&kernel, &a).await;
    let pre = unowned(&assembled(&kernel, &a).await[1..]);
    let complete = pre.len() - 1;
    let turns: Vec<i64> = base
        .iter()
        .filter(|r| r.get("role").is_some() && r.get("kind").is_none())
        .filter_map(|r| r["id"].as_i64())
        .collect();
    let ts = json!(crate::helpers::now_secs());
    let mut attrs = json!({"ts": ts});
    if let Some(off) = crate::row_attrs::offset_at(&ts) {
        attrs[crate::row_attrs::UTC_OFFSET] = off;
    }

    // an incoming row with no assistant row
    let orphan_in = json!({"id": kernel.mint_id(), "role": "user",
                           "content": "ORPHAN-INCOMING", "attributes": attrs});
    let mut rows = base.clone();
    rows.push(orphan_in);
    let shown = replayed(&kernel, "rs6_incoming", &json!({}), &rows).await;
    assert_eq!(shown[..complete], pre[..complete], "the complete rows");
    assert_eq!(shown.len(), pre.len() + 1);
    assert_eq!(shown[complete]["content"], json!("ORPHAN-INCOMING"));
    assert_eq!(shown[complete]["role"], json!("user"));

    // a fold row with no view row, built by hand (no fold writes one: RS5): shown after
    // the listed rows, its members still whole
    let fid = kernel.mint_id();
    let fold = crate::folds::make_fold(
        &turns[..2],
        "ORPHAN-FOLD",
        "ORPHAN-FOLD body",
        false,
        fid,
        turns[0],
        1,
        None,
    );
    let mut rows = base.clone();
    rows.push(fold);
    let shown = replayed(&kernel, "rs6_fold", &json!({}), &rows).await;
    assert_eq!(
        shown[..complete],
        pre[..complete],
        "the complete rows, members included"
    );
    assert_eq!(shown.len(), pre.len() + 1);
    assert_eq!(
        shown[complete]["id"],
        json!(fid),
        "the orphan fold row is last"
    );
    assert!(shown[complete]["content"]
        .as_str()
        .is_some_and(|c| c.contains("ORPHAN-FOLD")));

    // a view row that names an absent fold row: a stale array, the store as stored
    let mut ids: Vec<Value> = turns
        .iter()
        .map(|id| json!({"id": id, "res": "full"}))
        .collect();
    ids.insert(0, json!({"id": kernel.mint_id(), "res": "fold"}));
    let view = json!({"id": kernel.mint_id(), "kind": "view", "ts": crate::helpers::now_secs(),
                      "budget": 1000, "why": "rs6", "ids": ids});
    let mut rows = base.clone();
    rows.push(view);
    let shown = replayed(&kernel, "rs6_view", &json!({}), &rows).await;
    assert_eq!(shown, pre, "a stale view array shows the store as stored");

    // and a pass over each does not fail
    for name in ["rs6_incoming", "rs6_fold", "rs6_view"] {
        let b = AgentId::from(name);
        let p = Scripted::new(PLAIN, vec![]);
        answered(&kernel, &b, &p, &format!("{name}_pass")).await;
        let sent = last(&p);
        let text = Value::Array(sent).to_string();
        if name == "rs6_view" {
            assert_eq!(
                crate::state::state_for(&b)
                    .view_source
                    .lock()
                    .unwrap()
                    .as_deref(),
                Some("store (stale array)")
            );
        }
        assert!(!text.is_empty());
        crate::state::drop_state(&b);
    }

    // a real fold writes its fold row and its view row together (RS5): the two are the
    // fold's only writes, the view row names the fold, and the store cut before them
    // shows what the chat showed before the fold
    answered(&kernel, &a, &p, "rs6_2").await;
    let before = store(&kernel, &a).await;
    let unfolded = unowned(&assembled(&kernel, &a).await[1..]);
    let turns: Vec<i64> = before
        .iter()
        .filter(|r| r.get("role").is_some() && r.get("kind").is_none())
        .filter_map(|r| r["id"].as_i64())
        .collect();
    let out = verb(
        &kernel,
        &a,
        json!({"type": "context_fold", "from": turns[2], "to": turns[3], "summary": "RS6-FOLD"}),
    )
    .await;
    let fid = out["folded"]
        .as_i64()
        .unwrap_or_else(|| panic!("context_fold refused: {out}"));
    let writes = fold_writes(&store(&kernel, &a).await, max_id(&before));
    let kinds: Vec<&str> = writes
        .iter()
        .map(|w| w["kind"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(kinds, ["fold", "view"], "the writes of a fold");
    assert_eq!(writes[0]["id"], json!(fid));
    assert!(
        writes[1]["ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&json!({"id": fid, "res": "fold"}))),
        "the view row names the fold: {}",
        writes[1]
    );
    let cut = replayed(&kernel, "rs6_cut", &json!({}), &before).await;
    assert_eq!(cut, unfolded, "the store cut before the fold's append");
    crate::state::drop_state(&a);
}

// ─── RS7 ─────────────────────────────────────────────────────────────────────

/// What the child of RS7 does as `role`: one clean turn on the file store in its folder,
/// its request written to `pre.json`; then a send that streams (or whose re-ask streams)
/// and never ends, `ready.json` written once it streams. It is killed there.
async fn rs7_child(role: &str) {
    let dir = child_dir();
    let s = site(&dir, "rs7_chat", json!({"model": PLAIN}), None).await;
    let p = Scripted::new(PLAIN, vec![]);
    answered(&s.kernel, &s.agent, &p, "rs7_pre").await;
    write_json(
        &dir.join("pre.json"),
        &json!({"request": last(&p), "rows": store(&s.kernel, &s.agent).await}),
    );
    let steps = if role == "rs7_reask" {
        vec![Step::says(FOREIGN), Step::hangs("STREAMING-THE-REASK")]
    } else {
        vec![Step::hangs("STREAMING-THE-ANSWER")]
    };
    let n = 1 + steps.len();
    p.push(steps);
    let (k, a, pp) = (Arc::clone(&s.kernel), s.agent.clone(), Arc::clone(&p));
    tokio::spawn(async move {
        let _ = generate(&k, &a, &pp, "rs7_killed").await;
    });
    super::tests_trace::until_asked(&p, n).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    write_json(
        &dir.join("ready.json"),
        &json!({"ids": s.kernel.rows.id_reserved()}),
    );
    futures_util::future::pending::<()>().await;
}

// RS7
/// A real `kill -9`: a child process runs a chat on a file store against a slow
/// endpoint and is killed while the answer streams, and once more while a re-ask
/// streams. The parent opens the same store: no row of that send, no repair step, and
/// the next send starts at attempt 1 with the pre-stop bytes.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_process_recovers() {
    if let Some(role) = child_role() {
        rs7_child(&role).await;
        return;
    }
    for role in ["rs7_stream", "rs7_reask"] {
        let tmp = TempDir::new().unwrap();
        let log = std::fs::File::create(tmp.path().join("child.log")).expect("log");
        let mut c = child("killed_process_recovers", role, tmp.path(), None, &[]);
        c.stdout(log.try_clone().expect("log")).stderr(log);
        let mut kid = c.spawn().expect("the child starts");
        let ready = tmp.path().join("ready.json");
        for _ in 0..600 {
            if ready.exists() {
                break;
            }
            if let Some(status) = kid.try_wait().expect("wait") {
                panic!(
                    "{role}: the child ended ({status}) before it streamed:\n{}",
                    std::fs::read_to_string(tmp.path().join("child.log")).unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ready.exists(), "{role}: the child never streamed");
        // SIGKILL: no destructor, no flush, no shutdown of anything
        kid.kill().expect("kill -9");
        let _ = kid.wait();
        let ids = read_json(&ready)["ids"].as_i64();
        let pre = read_json(&tmp.path().join("pre.json"));
        let pre_req: Vec<Value> = pre["request"].as_array().cloned().unwrap();

        let s = site(tmp.path(), "rs7_chat", json!({"model": PLAIN}), ids).await;
        let rows = store(&s.kernel, &s.agent).await;
        assert_eq!(
            &Value::Array(rows.clone()),
            &pre["rows"],
            "{role}: no row of the send"
        );
        let rows_before = rows.clone();
        let text = Value::Array(rows).to_string();
        for dead in ["STREAMING-THE-ANSWER", "STREAMING-THE-REASK", "invoke name"] {
            assert!(!text.contains(dead), "{role}: {dead:?} is stored");
        }
        let p = Scripted::new(PLAIN, vec![]);
        answered(&s.kernel, &s.agent, &p, "rs7_after").await;
        let asked = turn_requests(&p);
        assert_eq!(asked.len(), 1, "{role}");
        let owed = owed_rows(&s.kernel, &s.agent, &rows_before).await;
        assert_eq!(
            asked[0].len(),
            pre_req.len() + 2 + owed,
            "{role}: no re-ask pair"
        );
        same_rows(&pre_req, &asked[0], role);
        s.stop();
    }
}

// ─── RS8-RS12 ────────────────────────────────────────────────────────────────

/// The pre-stop H bytes of a chat that went D -> H, and the restart that resolves its
/// type again: hermes, the pre-stop bytes, no D form.
async fn restart_as_hermes(tag: &str, before: Value, after: Value, way: &str) {
    let tmp = TempDir::new().unwrap();
    let s = site(tmp.path(), tag, before, None).await;
    let p = match way {
        "row" => Scripted::new(
            PLAIN,
            vec![Step::says(&format!("ok\n{DONE}")).named_model(QWEN)],
        ),
        "record" => Scripted::new(PLAIN, vec![Step::says(&format!("ok\n{DONE}"))]),
        _ => Scripted::new(QWEN, vec![]),
    };
    answered(&s.kernel, &s.agent, &p, &format!("{tag}_0")).await;
    if way == "record" {
        set_model(&s.kernel, &s.agent, QWEN).await;
        p.set_model(QWEN);
    }
    p.push(vec![
        Step::says(&format!("ok\n{H_DONE}")),
        Step::says(&format!("ok\n{H_DONE}")),
    ]);
    answered(&s.kernel, &s.agent, &p, &format!("{tag}_1")).await;
    answered(&s.kernel, &s.agent, &p, &format!("{tag}_2")).await;
    let last_rows = traces_of(&s.kernel, &s.agent, &format!("{tag}_2")).await;
    assert!(
        last_rows.iter().all(|r| r["type"] == json!(hermes::NAME)),
        "{way}: the pre-stop pass is H: {last_rows:?}"
    );
    let pre = last(&p);
    let ids = s.stop();

    let s = site(tmp.path(), tag, after, ids).await;
    let p = Scripted::new(QWEN, vec![]);
    answered(&s.kernel, &s.agent, &p, &format!("{tag}_after")).await;
    let rows = traces_of(&s.kernel, &s.agent, &format!("{tag}_after")).await;
    assert!(
        rows.iter().all(|r| r["type"] == json!(hermes::NAME)),
        "{way}: {rows:?}"
    );
    let now = last(&p);
    same_rows(&pre, &now, way);
    for m in &now[1..] {
        let c = m["content"].as_str().unwrap_or("");
        assert!(!c.contains("<call name="), "{way}: D's form in {c}");
    }
    assert!(
        now.iter()
            .any(|m| m["content"].as_str().is_some_and(|c| c.contains(H_DONE))),
        "{way}: the stored calls in H's form"
    );
    s.stop();
}

// RS8
/// A restart after a switch D -> H, three ways: the record's `model` names the hermes
/// id; no record `model` and the provider's `models` answers it as `default`; no record
/// `model` and no `models` answer (the newest row's `attributes.model`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_after_type_switch() {
    restart_as_hermes(
        "rs8_record",
        json!({"model": PLAIN}),
        json!({"model": QWEN}),
        "record",
    )
    .await;
    restart_as_hermes(
        "rs8_models",
        json!({"provider_id": "prov"}),
        json!({"provider_id": "prov"}),
        "models",
    )
    .await;
    restart_as_hermes("rs8_row", json!({}), json!({}), "row").await;
}

// RS9
/// A restart after a same-family switch (qwen2.5 -> qwen3, both hermes): hermes again,
/// the pre-stop bytes and links, no `model_switched`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_after_same_type_switch() {
    let tmp = TempDir::new().unwrap();
    let tag = "rs9";
    let s = site(tmp.path(), tag, json!({"model": QWEN25}), None).await;
    let p = Scripted::new(QWEN25, vec![]);
    answered(&s.kernel, &s.agent, &p, "rs9_0").await;
    answered(&s.kernel, &s.agent, &p, "rs9_1").await;
    set_model(&s.kernel, &s.agent, QWEN).await;
    p.set_model(QWEN);
    answered(&s.kernel, &s.agent, &p, "rs9_2").await;
    let pre = last(&p);
    let pre_links = chain(&s.agent, hermes::NAME);
    let ids = s.stop();

    let s = site(tmp.path(), tag, json!({"model": QWEN}), ids).await;
    let p = Scripted::new(QWEN, vec![]);
    answered(&s.kernel, &s.agent, &p, "rs9_after").await;
    let rows = traces_of(&s.kernel, &s.agent, "rs9_after").await;
    assert!(
        !names(&rows).contains(&"model_switched".to_string()),
        "{rows:?}"
    );
    assert!(
        rows.iter().all(|r| r["type"] == json!(hermes::NAME)),
        "{rows:?}"
    );
    let now = last(&p);
    same_rows(&pre, &now, tag);
    let k = pre.len() - 1;
    assert_eq!(chain(&s.agent, hermes::NAME)[..k], pre_links[..k]);
    s.stop();
}

/// A test adapter whose reply wrapper changes with its rev; every other text it renders
/// the same at every rev.
struct Wrapper {
    rev: u32,
}

impl Renderer for Wrapper {
    fn name(&self) -> &'static str {
        "wrap_test"
    }
    fn rev(&self) -> u32 {
        self.rev
    }
    fn render(&self, text: &str) -> String {
        if text.starts_with("[reply to ") {
            format!("<reply-r{0}>{text}</reply-r{0}>", self.rev)
        } else {
            format!("~{text}")
        }
    }
}

// RS10
/// A restart into a build with another adapter rev: the epoch differs, the render is
/// cold, and the messages that differ from the pre-stop bytes are exactly the ones that
/// hold a reply block (a list written here); every other message is byte-equal.
#[test]
fn adapter_rev_change_is_expected_diff() {
    let rows: Vec<(i64, &str)> = vec![
        (1, "Привіт"),
        (2, "читаю"),
        (3, "[reply to 11 · notes read]\nтекст"),
        (4, "далі"),
        (5, "[reply to 12 · notes write]\n{\"ok\": true}"),
        (6, "дякую"),
    ];
    let m = conv(&rows, "go");
    let (old, new) = (Wrapper { rev: 1 }, Wrapper { rev: 2 });
    let at = |r: &dyn Renderer| Opts {
        epoch: epoch_of(RULES_JSON.as_bytes(), &[&decl_of(r)]),
        ..Opts::default()
    };
    let (o1, o2) = (at(&old), at(&new));
    assert_ne!(o1.epoch, o2.epoch);
    let before = <RenderCache>::new();
    run(&before, &old, &m, &o1);
    let pre = run(&before, &old, &m, &o1);
    drop(before);
    let after = <RenderCache>::new();
    let cold = run(&after, &new, &m, &o2);
    assert!(cold.cold);
    assert_eq!(cold.rows, rows.len());
    let differ: Vec<usize> = (0..m.len())
        .filter(|&i| cold.messages[i] != pre.messages[i])
        .collect();
    // the messages holding a reply block: rows 3 and 5, at positions 3 and 5
    assert_eq!(differ, vec![3, 5]);
}

/// The shipped rule list with one rule put first.
fn with_first(rule: Value) -> super::rules::Rules {
    let mut v: Vec<Value> = serde_json::from_str(RULES_JSON).expect("rules");
    v.insert(0, rule);
    load(&serde_json::to_string_pretty(&v).expect("rules text")).expect("the list loads")
}

// RS11
/// A restart into a build with another rule list: the same model id moves hermes ->
/// default, and default -> hermes. The render is cold; the messages that differ are
/// exactly the ones holding a call, a reply block or the placeholder form, each equal
/// to the other type's golden; the rest byte-equal. `model_switched` is written when the
/// newest trace row names the old type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rule_change_is_expected_diff() {
    let call = r#"<call name="send"><routing><target_id>notes</target_id></routing><payload><type>read</type></payload></call>"#;
    let placeholder = format!("Почни з {}", super::default::FORM_OPEN);
    let rows: Vec<(i64, String)> = vec![
        (1, "Привіт".to_string()),
        (2, format!("читаю {call}")),
        (3, "[reply to 11 · notes read]\nтекст".to_string()),
        (4, "лише проза".to_string()),
        (5, placeholder),
        (6, "дякую".to_string()),
    ];
    let m = conv(&refs(&rows), "go");
    // the messages holding a call (2), a reply block (3) or the placeholder form (5)
    let expected = vec![2, 3, 5];
    for (id, rule) in [
        (QWEN, json!({"contains": "qwen3-8b", "type": "default"})),
        (
            "llama-3.1-8b-instruct",
            json!({"contains": "llama-3.1-8b", "type": "hermes"}),
        ),
    ] {
        let next = with_first(rule);
        let (was, now) = (type_of(shipped(), id), type_of(&next, id));
        assert_ne!(was, now, "{id}");
        let decls: Vec<&super::Decl<'_>> = super::REGISTRY.iter().collect();
        let o1 = Opts {
            epoch: epoch_of(RULES_JSON.as_bytes(), &decls),
            ..Opts::default()
        };
        let o2 = Opts {
            epoch: epoch_of(next.source.as_bytes(), &decls),
            ..Opts::default()
        };
        let before = <RenderCache>::new();
        let pre = run(&before, super::of(was).calls, &m, &o1);
        drop(before);
        let after = <RenderCache>::new();
        let cold = run(&after, super::of(now).calls, &m, &o2);
        assert!(cold.cold, "{id}");
        let differ: Vec<usize> = (0..m.len())
            .filter(|&i| cold.messages[i] != pre.messages[i])
            .collect();
        assert_eq!(differ, expected, "{id}: {was} -> {now}");
        for &i in &differ {
            let src = m[i]["content"].as_str().unwrap();
            assert_eq!(
                cold.messages[i]["content"],
                json!(super::of(now).calls.render(src)),
                "{id}: message {i} is the other type's golden"
            );
        }
    }

    // the switch across the restart is traced from the newest trace row
    let tmp = TempDir::new().unwrap();
    let (kernel, _) = crate::tests::mk_kernel(&tmp, "rs11_base").await;
    let a = chat_on(
        &kernel,
        "rs11_chat",
        fantastic_ram_store::test_support::MEMORY,
        Some(QWEN),
    )
    .await;
    let p = Scripted::new(QWEN, vec![]);
    answered(&kernel, &a, &p, "rs11_h").await;
    restart(&kernel, &a);
    // the new build maps the chat's model to `default`: a `default` id stands for it
    set_model(&kernel, &a, PLAIN).await;
    p.set_model(PLAIN);
    answered(&kernel, &a, &p, "rs11_d").await;
    let rows = traces_of(&kernel, &a, "rs11_d").await;
    assert_eq!(names(&rows)[0], "model_switched", "{rows:?}");
    assert_eq!(
        (&rows[0]["from"], &rows[0]["to"]),
        (&json!("hermes"), &json!("default"))
    );
    crate::state::drop_state(&a);
}

/// THE FIXTURE CHECK of the recorded prefix files: `text` (a recording) against the
/// adapter `r` and the shipped rule file now. A body that changed under an unchanged
/// `rev` fails naming `rev()`; another `rev` or rule file asks for a new recording.
fn check_recording(text: &str, r: &dyn Renderer) -> Result<(), String> {
    let (rev, digest, body) = recorded(text);
    if rev != r.rev() {
        return Err(format!(
            "{}: recorded under rev {rev}, the adapter is at rev {}: record the prefix files \
             again (`tests_determinism::record_prefix`)",
            r.name(),
            r.rev()
        ));
    }
    if digest != rules_digest() {
        return Err(format!(
            "{}: recorded under another rule file: record the prefix files again \
             (`tests_determinism::record_prefix`)",
            r.name()
        ));
    }
    if body != rendered_history(r) {
        return Err(format!(
            "{}: the rendered bytes changed under an unchanged rev() {rev}: raise rev()",
            r.name()
        ));
    }
    Ok(())
}

/// A recording of `body` with the header of `rev` and the shipped rule file.
fn recording(rev: u32, body: &str) -> String {
    format!(
        "# recorded by a test (#766)\n# rev: {rev}\n# rules_sha256: {}\n# commit: test\n{body}",
        rules_digest()
    )
}

// RS12
/// A silent change: changed bytes under an unchanged `rev` fail naming `rev()`; a
/// changed `rev` fails asking for a new recording; equal bytes and `rev` pass.
#[test]
fn silent_change_is_caught() {
    for r in [&Tags as &dyn Renderer, &Hermes] {
        let body = rendered_history(r);
        assert_eq!(
            check_recording(&recording(r.rev(), &body), r),
            Ok(()),
            "{}",
            r.name()
        );
        let changed = check_recording(&recording(r.rev(), &format!("{body}x")), r)
            .expect_err("changed bytes under an unchanged rev");
        assert!(changed.contains("raise rev()"), "{changed}");
        let other = check_recording(&recording(r.rev() + 1, &body), r).expect_err("another rev");
        assert!(other.contains("record the prefix files again"), "{other}");
    }
    // the checked-in files, when recorded
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/model_format/res/test");
    for (r, file) in [
        (&Tags as &dyn Renderer, "prefix_d.txt"),
        (&Hermes, "prefix_h.txt"),
    ] {
        let text = std::fs::read_to_string(dir.join(file)).unwrap_or_else(|e| {
            panic!("{file}: recorded by W4's last step (section 3, Fixtures): {e}")
        });
        assert_eq!(check_recording(&text, r), Ok(()), "{file}");
    }
}

// ─── RS13, RS14 ──────────────────────────────────────────────────────────────

/// One environment of RS13: (role suffix, variables, another cwd).
type Env = (&'static str, Vec<(&'static str, &'static str)>, bool);

/// The environments of RS13 a child runs under: (role suffix, variables, another cwd);
/// a locale or a zone this system does not have is left out, and said so.
fn rs13_envs() -> Vec<Env> {
    let mut out = Vec::new();
    if super::tests_determinism::locale_known("uk_UA.UTF-8") {
        out.push(("lc_uk", vec![("LC_ALL", "uk_UA.UTF-8")], false));
    } else {
        eprintln!("RS13: the locale uk_UA.UTF-8 is not on this system: skipped");
    }
    for (role, tz) in [
        ("tz_kiritimati", "Pacific/Kiritimati"),
        ("tz_los_angeles", "America/Los_Angeles"),
    ] {
        if super::tests_determinism::zone_known(tz) {
            out.push((role, vec![("TZ", tz)], false));
        } else {
            eprintln!("RS13: the zone {tz} is not on this system: skipped");
        }
    }
    out.push(("cwd", vec![], true));
    out
}

/// RS13's child: the chat of its folder started again, one send; its request, links and
/// trace events written to `out.json`.
async fn rs13_child(role: &str) {
    let (kind, model) = if role.ends_with("hermes") {
        ("hermes", QWEN)
    } else {
        ("default", PLAIN)
    };
    let dir = child_dir();
    let s = site(&dir, "rs13_chat", json!({"model": model}), child_ids()).await;
    let p = Scripted::new(model, vec![]);
    answered(&s.kernel, &s.agent, &p, "rs13_child").await;
    let events = names(&traces_of(&s.kernel, &s.agent, "rs13_child").await);
    write_json(
        &dir.join("out.json"),
        &json!({"request": last(&p), "links": hex(&chain(&s.agent, kind)), "events": events}),
    );
    s.stop();
}

// RS13
/// The chat of RS1 stopped, then started by a child process under another locale,
/// another time zone and another working directory: the pre-stop bytes, `ts` heads
/// included, and links; the first pass is cold only because the cache is new.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_other_locale_zone_and_cwd() {
    if let Some(role) = child_role() {
        rs13_child(&role).await;
        return;
    }
    for (kind, model) in KINDS {
        let tmp = TempDir::new().unwrap();
        let s = site(tmp.path(), "rs13_chat", json!({"model": model}), None).await;
        let p = Scripted::new(model, vec![]);
        for i in 0..3 {
            answered(&s.kernel, &s.agent, &p, &format!("rs13_{i}")).await;
        }
        let pre = last(&p);
        let pre_links = hex(&chain(&s.agent, kind));
        let ids = s.stop();
        let elsewhere = TempDir::new().unwrap();
        for (env, vars, cwd) in rs13_envs() {
            let copy = TempDir::new().unwrap();
            copy_tree(tmp.path(), copy.path());
            let role = format!("rs13_{env}_{kind}");
            let mut c = child(
                "restart_other_locale_zone_and_cwd",
                &role,
                copy.path(),
                ids,
                &vars,
            );
            if cwd {
                c.current_dir(elsewhere.path());
            }
            run_child(c, &role);
            let out = read_json(&copy.path().join("out.json"));
            let now: Vec<Value> = out["request"].as_array().cloned().unwrap();
            same_rows(&pre, &now, &role);
            let links: Vec<String> = out["links"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            let k = pre.len() - 1;
            assert_eq!(links[..k], pre_links[..k], "{role}: links");
            assert!(
                out["events"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("render_cold")),
                "{role}: {}",
                out["events"]
            );
        }
    }
}

/// The ids of the rows a view shows, in order.
fn view_ids(msgs: &[Value]) -> Vec<i64> {
    msgs.iter().filter_map(|m| m["id"].as_i64()).collect()
}

// RS14
/// Store reload order: a conversation with fold rows, view rows, a `keep`, a recall and a
/// `mark`; a stop right after a fold row was minted, one right after a view row, one
/// right after a recall. The rows read back in ascending id order; the first view after
/// the restart shows the pre-stop ids in order, with equal bytes and links; a row minted
/// after the restart has a higher id than every row before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_order_kept() {
    let tmp = TempDir::new().unwrap();
    let tag = "rs14";
    let fields = json!({"model": PLAIN});
    let mut s = site(tmp.path(), tag, fields.clone(), None).await;
    let p = Scripted::new(PLAIN, vec![]);
    for i in 0..4 {
        answered(&s.kernel, &s.agent, &p, &format!("rs14_{i}")).await;
    }
    let rows = store(&s.kernel, &s.agent).await;
    let turns: Vec<i64> = rows
        .iter()
        .filter(|r| r.get("role").is_some() && r.get("kind").is_none())
        .filter_map(|r| r["id"].as_i64())
        .collect();
    let (keep, mark) = (s.kernel.mint_id(), s.kernel.mint_id());
    append_raw(
        &s.kernel,
        &s.agent,
        &[
            json!({"id": keep, "kind": "keep", "of": 11, "summary": "kept"}),
            json!({"id": mark, "kind": "mark", "ts": crate::helpers::now_secs()}),
        ],
    )
    .await;
    for point in ["fold", "view", "recall"] {
        match point {
            "fold" => {
                let out = verb(
                    &s.kernel,
                    &s.agent,
                    json!({"type": "context_fold", "from": turns[2], "to": turns[3], "summary": "FOLD-RS14"}),
                )
                .await;
                assert!(out["folded"].is_i64(), "context_fold refused: {out}");
            }
            "view" => {
                let shown = assembled(&s.kernel, &s.agent).await;
                let ids: Vec<Value> = view_ids(&shown)
                    .into_iter()
                    .map(|id| json!({"id": id, "res": "full"}))
                    .collect();
                as_agent(
                    s.agent.clone(),
                    crate::memory::save_view(&s.agent, &s.kernel, &[], &ids, 1000, "rs14"),
                )
                .await
                .expect("view saved");
            }
            _ => {
                let out = verb(
                    &s.kernel,
                    &s.agent,
                    json!({"type": "context_recall", "dry": true}),
                )
                .await;
                assert!(out.get("error").is_none(), "{out}");
            }
        }
        let pre_rows = store(&s.kernel, &s.agent).await;
        let pre_view = assembled(&s.kernel, &s.agent).await;
        let ids = s.stop();
        s = site(tmp.path(), tag, fields.clone(), ids).await;
        let back = store(&s.kernel, &s.agent).await;
        let order: Vec<i64> = back.iter().filter_map(|r| r["id"].as_i64()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{point}: {order:?}");
        assert_eq!(back, pre_rows, "{point}: the rows read back");
        let view = assembled(&s.kernel, &s.agent).await;
        assert_eq!(
            view_ids(&view),
            view_ids(&pre_view),
            "{point}: the view's ids"
        );
        assert_eq!(view[1..], pre_view[1..], "{point}: the view's bytes");
        let new = save(
            &s,
            &[json!({"role": "user", "content": format!("after the {point} stop")})],
        )
        .await[0];
        assert!(
            new > max_id(&back),
            "{point}: {new} is not above {}",
            max_id(&back)
        );
    }
    // and the links of a pass equal those of the same pass before a stop
    answered(&s.kernel, &s.agent, &p, "rs14_links").await;
    let pre = last(&p);
    let pre_links = chain(&s.agent, "default");
    let ids = s.stop();
    let s = site(tmp.path(), tag, fields, ids).await;
    let p = Scripted::new(PLAIN, vec![]);
    answered(&s.kernel, &s.agent, &p, "rs14_links_after").await;
    same_rows(&pre, &last(&p), "links");
    let k = pre.len() - 1;
    assert_eq!(chain(&s.agent, "default")[..k], pre_links[..k]);
    s.stop();
}

// RS14, fold id order (owner 2026-11-25)
/// A row appended while a fold's summarizer runs keeps its place across a restart: the
/// fold's ids are minted in the one append that writes it with its view row
/// (`memory::save_view`), so they are above that row's; the rows read back, the view's
/// ids and its bytes equal the pre-stop ones; a fold written after it gets higher ids.
/// The fold strategy's request shows the ids its folds are stored under.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_during_summary_keeps_its_place() {
    let tmp = TempDir::new().unwrap();
    let tag = "rs14_mid";
    let fields = json!({"model": PLAIN});
    let s = site(tmp.path(), tag, fields.clone(), None).await;
    let p = Scripted::new(PLAIN, vec![]);
    for i in 0..4 {
        answered(&s.kernel, &s.agent, &p, &format!("{tag}_{i}")).await;
    }
    let turns: Vec<i64> = store(&s.kernel, &s.agent)
        .await
        .iter()
        .filter(|r| r.get("role").is_some() && r.get("kind").is_none())
        .filter_map(|r| r["id"].as_i64())
        .collect();
    // context_fold, its summarizer held until a row is appended
    let script = crate::tests::script(
        &s.agent,
        vec![crate::tests::pass("the work, folded\n\nwhat was read.")],
        Duration::ZERO,
    );
    let gate = script.hold_next();
    let fold = {
        let (k, a) = (Arc::clone(&s.kernel), s.agent.clone());
        let payload = json!({"type": "context_fold", "from": turns[2], "to": turns[3]});
        tokio::spawn(async move { verb(&k, &a, payload).await })
    };
    assert!(
        crate::tests::until_passes(&script, 1).await,
        "the summarizer was never asked"
    );
    let mid = save(
        &s,
        &[json!({"role": "user", "content": "written while the summary was"})],
    )
    .await[0];
    gate.notify_one();
    let out = fold.await.expect("the fold ran");
    let fid = out["folded"]
        .as_i64()
        .unwrap_or_else(|| panic!("context_fold refused: {out}"));
    assert!(
        fid > mid,
        "the fold's id {fid} is not above the row's {mid}"
    );
    let pre_rows = store(&s.kernel, &s.agent).await;
    let at = |id: i64| pre_rows.iter().position(|r| r["id"] == json!(id)).unwrap();
    assert!(at(mid) < at(fid), "the row is stored before the fold");
    assert_eq!(pre_rows[at(fid) + 1]["kind"], json!("view"), "{pre_rows:?}");
    let view_row = &pre_rows[at(fid) + 1];
    assert!(view_row["id"].as_i64().unwrap() > fid);
    assert!(
        view_row["ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == json!(fid)),
        "the view names the minted id: {view_row}"
    );
    let head = format!("{}{fid} · ", crate::folds::FOLD_TAG);
    assert!(
        pre_rows[at(fid)]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with(&head)),
        "{}",
        pre_rows[at(fid)]
    );
    let order: Vec<i64> = pre_rows.iter().filter_map(|r| r["id"].as_i64()).collect();
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "append order is id order: {order:?}"
    );
    let pre_view = assembled(&s.kernel, &s.agent).await;
    assert!(view_ids(&pre_view).contains(&fid));
    let ids = s.stop();
    let s = site(tmp.path(), tag, fields.clone(), ids).await;
    let back = store(&s.kernel, &s.agent).await;
    assert_eq!(back, pre_rows, "the rows read back");
    let view = assembled(&s.kernel, &s.agent).await;
    assert_eq!(view_ids(&view), view_ids(&pre_view), "the view's ids");
    assert_eq!(view[1..], pre_view[1..], "the view's bytes");
    // a fold written after it gets higher ids
    let out = verb(
        &s.kernel,
        &s.agent,
        json!({"type": "context_fold", "from": turns[5], "to": turns[6], "summary": "LATER"}),
    )
    .await;
    let later = out["folded"]
        .as_i64()
        .unwrap_or_else(|| panic!("context_fold refused: {out}"));
    assert!(
        later > max_id(&back),
        "{later} is not above {}",
        max_id(&back)
    );
    s.stop();

    // the fold strategy: the turn's request shows its folds under their stored ids
    let tag = "rs14_mid_auto";
    let fields = json!({"model": PLAIN, "context_strategy": "fold", "context_window": 4000,
        "output_reserve": 0});
    let s = site(tmp.path(), tag, fields.clone(), None).await;
    let mut rows = vec![json!({"role": "user", "content": "GOAL"})];
    for t in ["a", "b", "c", "d", "e", "f"] {
        rows.extend(chain_rows(&s.kernel, &format!("{tag}_{t}")));
    }
    save(&s, &rows).await;
    let before = max_id(&store(&s.kernel, &s.agent).await);
    let p = Scripted::new(PLAIN, vec![]);
    let _ = generate(&s.kernel, &s.agent, &p, &format!("{tag}_turn")).await;
    let pre_rows = store(&s.kernel, &s.agent).await;
    let folds: Vec<&Value> = pre_rows
        .iter()
        .filter(|r| r["kind"] == json!("fold"))
        .collect();
    assert!(!folds.is_empty(), "the projection folded nothing");
    let fold_ids: Vec<i64> = folds.iter().filter_map(|r| r["id"].as_i64()).collect();
    assert!(
        fold_ids.iter().all(|&id| id > before),
        "{fold_ids:?} above {before}"
    );
    for f in &folds {
        let head = format!("{}{} · ", crate::folds::FOLD_TAG, f["id"]);
        assert!(
            f["content"].as_str().is_some_and(|c| c.starts_with(&head)),
            "{f}"
        );
        for r in f["refs"].as_array().unwrap() {
            assert!(
                r.as_i64().is_some_and(|id| id > 0),
                "a provisional ref: {f}"
            );
        }
    }
    let order: Vec<i64> = pre_rows.iter().filter_map(|r| r["id"].as_i64()).collect();
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "append order is id order: {order:?}"
    );
    // the request carries no ids: each fold the projection's view row names is shown
    // under its stored id, and no fold head names a provisional one
    let req = turn_requests(&p).last().cloned().expect("a turn request");
    let text: Vec<&str> = req.iter().filter_map(|m| m["content"].as_str()).collect();
    let provisional = format!("{}-", crate::folds::FOLD_TAG);
    assert!(
        text.iter().all(|c| !c.contains(&provisional)),
        "a provisional fold id in the request: {req:#?}"
    );
    let view_row = pre_rows
        .iter()
        .rev()
        .find(|r| r["kind"] == json!("view"))
        .expect("the projection's view row");
    let named: Vec<i64> = view_row["ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_i64())
        .filter(|id| fold_ids.contains(id))
        .collect();
    assert!(!named.is_empty(), "the view names no fold: {view_row}");
    for id in named {
        let head = format!("{}{id} · ", crate::folds::FOLD_TAG);
        assert!(
            text.iter().any(|c| c.contains(&head)),
            "fold {id} not shown: {req:#?}"
        );
    }
    let ids = s.stop();
    let s = site(tmp.path(), tag, fields, ids).await;
    assert_eq!(
        store(&s.kernel, &s.agent).await,
        pre_rows,
        "the rows read back"
    );
    s.stop();
}

// ─── RS15-RS18 ───────────────────────────────────────────────────────────────

// RS15
/// Two chats in one process; A is shut down and booted again while B stays: B's next
/// pass renders no old row and its bytes are unchanged; A's first pass is cold and
/// byte-equal to A's pre-stop bytes; A's old cache was freed with its state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_chat_restarts_other_warm() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _) = crate::tests::mk_kernel(&tmp, "rs15_base").await;
    let mem = fantastic_ram_store::test_support::MEMORY;
    let a = chat_on(&kernel, "rs15_a", mem, Some(QWEN)).await;
    let b = chat_on(&kernel, "rs15_b", mem, Some(QWEN)).await;
    let (pa, pb) = (Scripted::new(QWEN, vec![]), Scripted::new(QWEN, vec![]));
    for i in 0..2 {
        answered(&kernel, &a, &pa, &format!("rs15_a{i}")).await;
        answered(&kernel, &b, &pb, &format!("rs15_b{i}")).await;
    }
    let (pre_a, pre_b) = (last(&pa), last(&pb));
    let b_before = crate::state::state_for(&b).render_cache.stats();
    let weak = Arc::downgrade(&crate::state::state_for(&a));
    restart(&kernel, &a);
    assert!(weak.upgrade().is_none(), "A's cache went with its state");
    assert_eq!(crate::state::state_for(&b).render_cache.stats(), b_before);

    answered(&kernel, &b, &pb, "rs15_b_next").await;
    let warm = traces_of(&kernel, &b, "rs15_b_next").await;
    let r = warm
        .iter()
        .find(|r| r["event"] == json!("render_warm"))
        .expect("B warm");
    assert_eq!(r["rows"], json!(2), "B renders only its new rows");
    same_rows(&pre_b, &last(&pb), "B");

    let n = with_ids(&assembled(&kernel, &a).await);
    answered(&kernel, &a, &pa, "rs15_a_next").await;
    let cold = traces_of(&kernel, &a, "rs15_a_next").await;
    let r = cold
        .iter()
        .find(|r| r["event"] == json!("render_cold"))
        .expect("A cold");
    assert_eq!(r["rows"], json!(n));
    same_rows(&pre_a, &last(&pa), "A");
    crate::state::drop_state(&a);
    crate::state::drop_state(&b);
}

// RS16
/// Three chats, each with its own bound: C's cold render exceeds C's bound while B's pass
/// holds its pins and A stands at its bound. C evicts only its own entries; A and B lose
/// none and their views are unchanged; C renders its evicted rows again byte-equal; each
/// chat's bytes are within its own bound once its pass ends.
#[test]
fn each_chat_evicts_its_own() {
    let r = Count::new("t");
    let (a, b, c) = (
        <RenderCache>::new(),
        <RenderCache>::new(),
        <RenderCache>::new(),
    );
    let ma = conv(&refs(&rows_n(1, 6, 50)), "go");
    let mb = conv(&refs(&rows_n(1, 6, 50)), "go");
    let mc = conv(&refs(&rows_n(1, 12, 200)), "go");
    // A at its bound
    let first_a = run(&a, &r, &ma, &Opts::default());
    let bound_a = a.stats().bytes;
    run(&a, &r, &ma, &Opts::bound(bound_a));
    let a_stats = a.stats();
    // B's pass holds its pins
    run(&b, &r, &mb, &Opts::default());
    let held = open(&b, &r, &mb, &Opts::bound(usize::MAX));
    let b_stats = b.stats();
    assert!(b_stats.pins > 0);
    // C's cold render is larger than its bound
    let bound_c = 4 * 250;
    let first_c = run(&c, &r, &mc, &Opts::bound(bound_c));
    assert!(
        c.stats().bytes <= bound_c,
        "C within its bound once its pass ended"
    );
    assert!(c.stats().entries < 12, "C evicted");
    assert_eq!(a.stats(), a_stats, "A lost nothing");
    assert_eq!(b.stats(), b_stats, "B lost nothing");
    let again_c = run(&c, &r, &mc, &Opts::bound(bound_c));
    assert_eq!(
        again_c.messages, first_c.messages,
        "C's evicted rows, again"
    );
    assert!(again_c.rows > 0);
    assert_eq!(again_c.messages, plain(&r, &mc));
    let b_done = finish(held);
    assert_eq!(b_done.messages, plain(&r, &mb));
    let a_next = run(&a, &r, &ma, &Opts::bound(bound_a));
    assert_eq!(a_next.rows, 0, "A's entries are all there");
    assert_eq!(a_next.messages, first_a.messages);
    assert!(a.stats().bytes <= bound_a);
}

/// Hermes rows larger together than [`SMALL`]: user texts and assistant calls with a body.
fn big_rows(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                json!({"role": "user", "content": format!("крок {i}: {}", "слово ".repeat(500))})
            } else {
                json!({"role": "assistant", "content": format!(
                    "<call name=\"send\"><routing><target_id>notes</target_id></routing><payload>\
                     <type>write</type><path>f{i}.md</path><bytes>{}</bytes></payload></call>",
                    "рядок коду;\n".repeat(300))})
            }
        })
        .collect()
}

/// The messages `assembled` shows, rendered for hermes with no cache: the unbounded
/// render the request must equal, row by row.
fn unbounded(assembled: &[Value]) -> Vec<Value> {
    plain(&Hermes, assembled)
}

/// The request's rows equal the unbounded render of the view it was built from.
fn rows_unbounded(req: &[Value], view: &[Value], what: &str) {
    let want = unbounded(view);
    let k = view.len() - 1;
    for i in 1..k {
        assert_eq!(req[i]["content"], want[i]["content"], "{what}: message {i}");
    }
}

// RS17
/// Eviction under an overridden bound (`render_cache_bytes` of 64 KiB on the record),
/// then a restart: before the stop entries are evicted and every view is byte-equal to
/// an unbounded render; after it the bound is read again from the record, the first pass
/// is cold and byte-equal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_under_override_then_restart() {
    let tmp = TempDir::new().unwrap();
    let tag = "rs17";
    let fields = json!({"model": QWEN, "render_cache_bytes": SMALL, "context_window": 262144});
    let s = site(tmp.path(), tag, fields.clone(), None).await;
    save(&s, &big_rows(10)).await;
    let p = Scripted::new(QWEN, vec![]);
    for i in 0..2 {
        let view = assembled(&s.kernel, &s.agent).await;
        answered(&s.kernel, &s.agent, &p, &format!("rs17_{i}")).await;
        rows_unbounded(&last(&p), &view, &format!("pass {i}"));
        let st = crate::state::state_for(&s.agent).render_cache.stats();
        assert!(st.bytes <= SMALL, "pass {i}: {} bytes", st.bytes);
        assert!(
            st.entries < with_ids(&view),
            "pass {i}: entries were evicted"
        );
    }
    let pre = last(&p);
    let ids = s.stop();
    let s = site(tmp.path(), tag, fields, ids).await;
    let meta = crate::helpers::agent_meta(&s.agent, &s.kernel);
    assert_eq!(
        super::cache::bound_of(&meta),
        SMALL,
        "the bound, from the record"
    );
    let p = Scripted::new(QWEN, vec![]);
    answered(&s.kernel, &s.agent, &p, "rs17_after").await;
    assert_eq!(
        render_row(&s, "rs17_after").await["event"],
        json!("render_cold")
    );
    same_rows(&pre, &last(&p), tag);
    assert!(crate::state::state_for(&s.agent).render_cache.stats().bytes <= SMALL);
    s.stop();
}

// RS18
/// `render_cache_bytes` rewritten while the process runs, then a restart: the new bound
/// holds from the chat's next pass and after the restart; the views are byte-equal
/// before the rewrite, after it and after the restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn override_rewritten_then_restart() {
    let tmp = TempDir::new().unwrap();
    let tag = "rs18";
    let fields = json!({"model": QWEN, "context_window": 262144});
    let s = site(tmp.path(), tag, fields.clone(), None).await;
    save(&s, &big_rows(10)).await;
    let p = Scripted::new(QWEN, vec![]);
    let view = assembled(&s.kernel, &s.agent).await;
    answered(&s.kernel, &s.agent, &p, "rs18_wide").await;
    let wide = last(&p);
    rows_unbounded(&wide, &view, "default bound");
    let st = crate::state::state_for(&s.agent).render_cache.stats();
    assert!(
        st.bytes > SMALL,
        "the default bound evicts nothing: {}",
        st.bytes
    );
    set(&s.kernel, &s.agent, &json!({"render_cache_bytes": SMALL})).await;
    let view = assembled(&s.kernel, &s.agent).await;
    answered(&s.kernel, &s.agent, &p, "rs18_small").await;
    let small = last(&p);
    rows_unbounded(&small, &view, "the rewritten bound");
    assert!(crate::state::state_for(&s.agent).render_cache.stats().bytes <= SMALL);
    same_rows(&wide, &small, "before and after the rewrite");
    let ids = s.stop();
    let mut after = fields;
    after["render_cache_bytes"] = json!(SMALL);
    let s = site(tmp.path(), tag, after, ids).await;
    let p = Scripted::new(QWEN, vec![]);
    answered(&s.kernel, &s.agent, &p, "rs18_after").await;
    same_rows(&small, &last(&p), "after the restart");
    same_rows(&wide, &last(&p), "the first view, after the restart");
    assert!(crate::state::state_for(&s.agent).render_cache.stats().bytes <= SMALL);
    s.stop();
}
