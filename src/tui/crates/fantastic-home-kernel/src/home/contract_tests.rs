//! THE CONTRACT REPLAY: `contract/home/v1/cases.json`, the `home` protocol v1
//! as data, replayed in order against one runner on `FakeOs::from_fixture`,
//! through `agent::call` — and through the kernel's `docs`, and against
//! `docs/home.md`. The TS client replays the same file
//! (`src/tui/web/canvas/tests/home_contract.test.ts`).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use fantastic_home::{Canvas, Config as HomeConfig, Mode, Row, Status};
use fantastic_kernel::agent::Agent;
use fantastic_kernel::bundle::{Bundle, BundleRegistry};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use super::agent::{self, reflect_call, HomeBundle};
use super::fake::FakeOs;
use super::runner::Runner;
use super::{wire, HomeOs, HANDLER_MODULE, ID, VERBS};

/// The corpus, as this OS reads it: on Windows every `/w/` is rooted at `C:/w/`
/// (the corpus's paths are POSIX, and `/w/x` is not absolute there).
fn corpus() -> Value {
    // crate → crates → tui → src → the repo root; embedded, so the test reads no file
    let text = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../contract/home/v1/cases.json"
    ));
    let doc: Value = serde_json::from_str(text).expect("contract/home/v1/cases.json");
    if cfg!(windows) {
        rooted(doc)
    } else {
        doc
    }
}

fn rooted(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("/w/", "C:/w/")),
        Value::Array(items) => Value::Array(items.into_iter().map(rooted).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k.replace("/w/", "C:/w/"), rooted(v)))
                .collect(),
        ),
        other => other,
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("not a list: {v}"))
        .iter()
        .map(|s| s.as_str().expect("a string").to_string())
        .collect()
}

/// Every value at a dotted path; `*` is any element (or any value of an object).
fn at<'a>(v: &'a Value, path: &str) -> Vec<&'a Value> {
    let mut here = vec![v];
    for step in path.split('.') {
        let mut next = Vec::new();
        for v in here {
            match (step, v) {
                ("*", Value::Array(items)) => next.extend(items.iter()),
                ("*", Value::Object(map)) => next.extend(map.values()),
                (k, Value::Array(items)) => {
                    if let Some(x) = k.parse::<usize>().ok().and_then(|i| items.get(i)) {
                        next.push(x);
                    }
                }
                (k, Value::Object(map)) => {
                    if let Some(x) = map.get(k) {
                        next.push(x);
                    }
                }
                _ => {}
            }
        }
        here = next;
    }
    here
}

/// The one value at a dotted path, `null` when there is none.
fn one(v: &Value, path: &str) -> Value {
    at(v, path)
        .first()
        .map(|x| (*x).clone())
        .unwrap_or(Value::Null)
}

/// One runner on the fixture's machine, every emit recorded.
struct Replay {
    runner: Arc<Runner>,
    os: Arc<FakeOs>,
    emitted: Arc<Mutex<Vec<Value>>>,
}

impl Replay {
    fn new(fixture: &Value) -> Replay {
        let os = Arc::new(FakeOs::from_fixture(fixture));
        let own = fixture["own_folder"]
            .as_str()
            .expect("fixture: own_folder")
            .to_string();
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&emitted);
        let runner = Runner::new(
            Arc::clone(&os) as Arc<dyn HomeOs>,
            HomeConfig::new(Mode::Lock).own_folder(own),
            Box::new(move |payload| sink.lock().unwrap().push(payload)),
            Box::new(|_, _| {}),
        );
        Replay {
            runner,
            os,
            emitted,
        }
    }

    async fn call(&self, payload: &Value) -> Value {
        if payload["type"] == json!("reflect") {
            return reflect_call(&self.runner, ID, None, payload);
        }
        agent::call(&self.runner, payload).await
    }

    async fn look(&self) {
        self.runner.look();
        self.runner.settle().await;
    }
}

const CASE_KEYS: [&str; 15] = [
    "name",
    "before",
    "call",
    "expect",
    "expect_keys",
    "expect_absent",
    "expect_refusal",
    "expect_same_state",
    "expect_same_seq",
    "expect_emits",
    "expect_spawned",
    "expect_terminated",
    "expect_forwarded",
    "after",
    "_doc",
];

/// Run every case in order; the reply of each, by name.
async fn replay(doc: &Value) -> Map<String, Value> {
    let r = Replay::new(&doc["fixture"]);
    let mut replies = Map::new();
    for case in doc["cases"].as_array().expect("cases") {
        let c = case.as_object().unwrap_or_else(|| panic!("a case: {case}"));
        let name = c["name"].as_str().expect("a case's name").to_string();
        for k in c.keys() {
            assert!(CASE_KEYS.contains(&k.as_str()), "{name}: unknown key `{k}`");
        }
        let call = &c["call"];
        assert!(call["type"].is_string(), "{name}: call.type");
        if let Some(before) = c.get("before") {
            assert_eq!(before, &json!(["look"]), "{name}: before");
            r.look().await;
        }
        let base = if c.contains_key("expect_same_state") || c.contains_key("expect_same_seq") {
            r.runner.settle().await;
            Some(r.call(&json!({"type": "list"})).await)
        } else {
            None
        };
        let emits_from = r.emitted.lock().unwrap().len();
        let (spawned, terminated, forwarded) = (
            r.os.spawned().len(),
            r.os.terminated().len(),
            r.os.forwarded().len(),
        );
        let got = r.call(call).await;
        let emitted: Vec<Value> = r.emitted.lock().unwrap()[emits_from..].to_vec();
        r.runner.settle().await;

        if let Some(keys) = c.get("expect_keys") {
            let have: Vec<String> = got
                .as_object()
                .unwrap_or_else(|| panic!("{name}: {got}"))
                .keys()
                .cloned()
                .collect();
            assert_eq!(have, strings(keys), "{name}: keys of {got}");
        }
        if let Some(expect) = c.get("expect") {
            for (path, want) in expect.as_object().expect("expect") {
                assert_eq!(&one(&got, path), want, "{name}: {path} of {got}");
            }
        }
        if let Some(absent) = c.get("expect_absent") {
            for (path, never) in absent.as_object().expect("expect_absent") {
                assert!(
                    !at(&got, path).contains(&never),
                    "{name}: {path} holds {never}: {got}"
                );
            }
        }
        if let Some(refusal) = c.get("expect_refusal") {
            assert!(got["error"].is_string(), "{name}: not refused: {got}");
            assert_eq!(got["reason"], refusal["reason"], "{name}: {got}");
            assert_eq!(one(&got, "arg"), refusal["arg"], "{name}: {got}");
            if let Some(part) = refusal.get("error_contains") {
                let part = part.as_str().expect("error_contains");
                assert!(
                    got["error"].as_str().is_some_and(|e| e.contains(part)),
                    "{name}: the error does not say {part:?}: {got}"
                );
            }
        } else {
            assert!(got.get("error").is_none(), "{name}: refused: {got}");
        }
        if c.contains_key("expect_same_state") {
            assert_eq!(Some(&got), base.as_ref(), "{name}: the state changed");
        }
        if c.contains_key("expect_same_seq") {
            assert_eq!(
                got["seq"],
                base.as_ref().unwrap()["seq"],
                "{name}: not the state's seq"
            );
        }
        if let Some(want) = c.get("expect_emits") {
            let types: Vec<String> = emitted
                .iter()
                .filter(|e| e["type"] != json!("state"))
                .map(|e| e["type"].as_str().unwrap_or("").to_string())
                .collect();
            assert_eq!(types, strings(want), "{name}: emitted {emitted:?}");
        }
        if let Some(want) = c.get("expect_spawned") {
            assert_eq!(
                r.os.spawned()[spawned..],
                strings(want)[..],
                "{name}: spawned"
            );
        }
        if let Some(want) = c.get("expect_terminated") {
            assert_eq!(
                r.os.terminated()[terminated..],
                strings(want)[..],
                "{name}: terminated"
            );
        }
        if let Some(want) = c.get("expect_forwarded") {
            let sent: Vec<Value> = r.os.forwarded()[forwarded..]
                .iter()
                .map(|f| json!({"port": f.port, "target": f.target, "payload": f.payload}))
                .collect();
            assert_eq!(&Value::Array(sent), want, "{name}: forwarded");
        }
        if let Some(after) = c.get("after") {
            let state = r.call(&json!({"type": "list"})).await;
            for (k, v) in after.as_object().expect("after") {
                match k.as_str() {
                    "problem_contains" => {
                        let part = v.as_str().expect("problem_contains");
                        assert!(
                            state["problem"].as_str().is_some_and(|p| p.contains(part)),
                            "{name}: the problem does not say {part:?}: {state}"
                        );
                    }
                    "expect" => {
                        for (path, want) in v.as_object().expect("after.expect") {
                            assert_eq!(
                                &one(&state, path),
                                want,
                                "{name}: after: {path} of {state}"
                            );
                        }
                    }
                    other => panic!("{name}: after: unknown key `{other}`"),
                }
            }
        }
        replies.insert(name, got);
    }
    replies
}

#[tokio::test]
async fn every_case_replays_against_the_agent() {
    let doc = corpus();
    let replies = replay(&doc).await;
    // golden: the state `first-look` answers is `states.valid[0]`
    assert_eq!(
        replies["first-look"], doc["states"]["valid"][0],
        "the golden state"
    );
    assert_eq!(doc["protocol"], json!(wire::PROTOCOL));
    assert_eq!(doc["version"], json!(wire::VERSION));
}

#[test]
fn every_verb_and_refusal_kind_has_a_case() {
    let doc = corpus();
    let cases = doc["cases"].as_array().unwrap();
    for verb in strings(&doc["verbs"]) {
        assert!(
            cases.iter().any(|c| c["call"]["type"] == json!(verb)),
            "no case calls {verb}"
        );
    }
    for reason in ["missing_arg", "bad_arg", "not_found", "unhandled"] {
        assert!(
            cases
                .iter()
                .any(|c| c["expect_refusal"]["reason"] == json!(reason)),
            "no case is refused {reason}"
        );
    }
    assert!(
        cases
            .iter()
            .any(|c| c["expect_refusal"]["arg"] == json!("version")),
        "no case refuses a version"
    );
}

#[test]
fn the_verbs_are_the_agents_and_its_docs() {
    let doc = corpus();
    let verbs = strings(&doc["verbs"]);
    let mut agent: Vec<&str> = VERBS.to_vec();
    agent.sort_unstable();
    let mut corpus_verbs: Vec<&str> = verbs.iter().map(String::as_str).collect();
    corpus_verbs.sort_unstable();
    assert_eq!(corpus_verbs, agent, "the corpus verbs are home::VERBS");
    let bundle = HomeBundle::new(Arc::new(FakeOs::new()), HomeConfig::new(Mode::Lock));
    let mut documented: Vec<String> = bundle
        .verb_docs()
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| *k != "boot" && *k != "shutdown")
        .cloned()
        .collect();
    documented.sort_unstable();
    let mut want = verbs.clone();
    want.sort_unstable();
    assert_eq!(
        documented, want,
        "the corpus verbs are the documented verbs"
    );
}

#[test]
fn the_events_are_the_emits_doc() {
    let doc = corpus();
    let bundle = HomeBundle::new(Arc::new(FakeOs::new()), HomeConfig::new(Mode::Lock));
    let emits: Vec<String> = bundle
        .emits_doc()
        .expect("emits")
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(strings(&doc["events"]), emits);
}

fn every_status() -> Vec<Status> {
    let canvas = Canvas {
        url: "http://127.0.0.1:1/".into(),
        port: 1,
    };
    vec![
        Status::Stopped,
        Status::Starting,
        Status::Stopping,
        Status::Running(canvas),
        Status::NoCanvas,
        Status::Unverified,
        Status::Missing,
    ]
}

#[test]
fn the_statuses_are_the_serde_names() {
    let doc = corpus();
    let names: Vec<String> = every_status()
        .iter()
        .map(|s| match serde_json::to_value(s).unwrap() {
            Value::String(n) => n,
            Value::Object(m) => m.keys().next().unwrap().clone(),
            other => panic!("a status serializes as {other}"),
        })
        .collect();
    assert_eq!(strings(&doc["statuses"]), names);
    let wire: Vec<&str> = every_status().iter().map(wire::status_name).collect();
    assert_eq!(names, wire, "the wire names are the serde names");
}

#[test]
fn the_actions_cover_every_row() {
    let doc = corpus();
    let mut seen = BTreeSet::new();
    for status in every_status() {
        for pid in [None, Some(1)] {
            for guid in [None, Some("g".to_string())] {
                for mode in [Mode::Lock, Mode::Canvas] {
                    let row = Row {
                        id: "x".into(),
                        guid: guid.clone(),
                        name: "x".into(),
                        path: "/x".into(),
                        last_seen: None,
                        status: status.clone(),
                        pid,
                    };
                    let actions = row.actions(mode);
                    // in the corpus's order
                    let order: Vec<String> = strings(&doc["actions"])
                        .into_iter()
                        .filter(|a| actions.contains(&a.as_str()))
                        .collect();
                    assert_eq!(order, actions, "{row:?} in {mode:?}");
                    seen.extend(actions.iter().map(|a| a.to_string()));
                }
            }
        }
    }
    let want: BTreeSet<String> = strings(&doc["actions"]).into_iter().collect();
    assert_eq!(seen, want);
}

/// A kernel holding `home`, the way the agent tests build one.
fn kernel() -> Arc<Kernel> {
    let mut k = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register(
        HANDLER_MODULE,
        HomeBundle::new(Arc::new(FakeOs::new()), HomeConfig::new(Mode::Lock)),
    );
    k.bundles = reg;
    let k = Arc::new(k);
    k.register(Agent::new(
        AgentId::from(ID),
        Some(HANDLER_MODULE.into()),
        None,
        Map::new(),
        std::path::PathBuf::new(),
        false,
    ));
    k
}

#[test]
fn docs_of_home_answer_every_verb_the_emits_and_the_notes() {
    let doc = corpus();
    let k = kernel();
    let r = fantastic_kernel::docs::docs_reply(&k, &json!({"of": "home", "tier": "detailed"}));
    let verbs: Vec<String> = r["verbs"]
        .as_object()
        .expect("verbs")
        .keys()
        .cloned()
        .collect();
    let mut want = strings(&doc["verbs"]);
    want.extend(["boot".to_string(), "shutdown".to_string()]);
    let (mut a, mut b) = (verbs.clone(), want);
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b, "{r}");
    for e in ["state", "focus"] {
        assert!(r["emits"].get(e).is_some(), "emits {e}: {r}");
    }
    let notes = &r["notes"];
    assert_eq!(notes["protocol"], json!("fantastic.home"));
    assert_eq!(notes["version"], json!(1));
    for k in ["host", "calls", "events", "origin", "compat", "doc"] {
        assert!(
            notes[k].as_str().is_some_and(|s| !s.is_empty()),
            "notes.{k}: {r}"
        );
    }
    // the oneliner of every client verb is its signature
    let short = fantastic_kernel::docs::docs_reply(&k, &json!({"of": "home"}));
    for (verb, line) in short["verbs"].as_object().unwrap() {
        let line = line.as_str().unwrap();
        if verb == "boot" || verb == "shutdown" {
            assert!(line.starts_with("Kernel lifecycle"), "{verb}: {line}");
        } else {
            assert!(
                line.starts_with("args: ") && line.ends_with("version:int?"),
                "{verb}: {line}"
            );
        }
    }
}

#[test]
fn home_md_names_every_verb_status_and_state_field() {
    let doc = corpus();
    let md = include_str!("../../docs/home.md");
    assert!(
        md.starts_with("> "),
        "home.md opens with a `> ` summary line"
    );
    for verb in strings(&doc["verbs"]) {
        assert!(
            md.contains(&format!("\n### `{verb}`\n")),
            "home.md has no `### `{verb}`` heading"
        );
    }
    for status in strings(&doc["statuses"]) {
        assert!(
            md.contains(&format!("`{status}`")),
            "home.md does not name {status}"
        );
    }
    let state = &doc["states"]["valid"][0];
    let row = &state["rows"][0];
    for field in state
        .as_object()
        .unwrap()
        .keys()
        .chain(row.as_object().unwrap().keys())
    {
        assert!(
            md.contains(&format!("`{field}`")),
            "home.md does not name the field {field}"
        );
    }
    for event in strings(&doc["events"]) {
        assert!(
            md.contains(&format!("`{event}`")),
            "home.md does not name the event {event}"
        );
    }
}
