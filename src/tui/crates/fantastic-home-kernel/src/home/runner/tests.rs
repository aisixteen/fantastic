//! The runner against a scripted machine (`FakeOs`).

use std::sync::{Arc, Mutex};

use fantastic_home::{Config as HomeConfig, Event, Mode};
use serde_json::{json, Value};

use super::Runner;
use crate::home::fake::{run, FakeOs, Folder, Forwarded};

type Kept = Arc<Mutex<Vec<(Vec<String>, Option<String>)>>>;

struct T {
    os: Arc<FakeOs>,
    runner: Arc<Runner>,
    emitted: Arc<Mutex<Vec<Value>>>,
    kept: Kept,
}

fn t() -> T {
    let os = Arc::new(FakeOs::new());
    let emitted = Arc::new(Mutex::new(Vec::new()));
    let kept: Kept = Arc::new(Mutex::new(Vec::new()));
    let (e, k) = (Arc::clone(&emitted), Arc::clone(&kept));
    let runner = Runner::new(
        Arc::clone(&os) as Arc<dyn crate::home::HomeOs>,
        HomeConfig::new(Mode::Lock),
        Box::new(move |v| e.lock().unwrap().push(v)),
        Box::new(move |p, s| k.lock().unwrap().push((p, s))),
    );
    T {
        os,
        runner,
        emitted,
        kept,
    }
}

/// A registry entry's `(folder, yaml)`.
fn entry(guid: &str, path: &str, pid: Option<u32>, port: Option<u16>) -> (String, String) {
    let mut yaml = format!("guid: {guid}\npath: {path}\nstored_at: 1\nlast_seen: 1760800000\n");
    if let Some(pid) = pid {
        yaml.push_str(&format!("pid: {pid}\n"));
    }
    if let Some(port) = port {
        yaml.push_str(&format!(
            "canvas: http://127.0.0.1:{port}/canvas_page/file/index.html\n"
        ));
    }
    (format!("f-{guid}"), yaml)
}

impl T {
    async fn look(&self) -> Value {
        self.runner.apply(Event::Tick { now_ms: 0 });
        self.runner.settle().await;
        self.runner.state()
    }

    async fn settle(&self) -> Value {
        self.runner.settle().await;
        self.runner.state()
    }

    fn stopped(&self, path: &str) {
        self.os.set_folder(
            path,
            Folder {
                is_fantastic: true,
                holder: None,
                run: None,
            },
        );
    }

    /// The folder held by `pid`, its canvas on `port` up, answering `guid`.
    fn running(&self, path: &str, pid: u32, port: u16, guid: &str) {
        self.os.set_folder(
            path,
            Folder {
                is_fantastic: true,
                holder: Some(pid),
                run: Some(run(pid, port)),
            },
        );
        self.os.set_canvas_up(port, true);
        self.os.set_guid(port, guid);
    }
}

fn row<'a>(state: &'a Value, id: &str) -> &'a Value {
    state["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(id))
        .unwrap_or_else(|| panic!("no row {id} in {state}"))
}

fn status(state: &Value, id: &str) -> String {
    row(state, id)["status"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn the_first_look_lists_every_entry_with_its_status() {
    let t = t();
    t.os.set_registry(vec![
        entry("g1", "/w/a", None, None),
        entry("g2", "/w/b", Some(7), Some(8100)),
        entry("g3", "/w/c", None, None),
    ]);
    t.stopped("/w/a");
    t.running("/w/b", 7, 8100, "g2");
    let before = t.runner.state();
    assert_eq!(before["listed"], json!(false));
    let s = t.look().await;
    assert_eq!(s["listed"], json!(true));
    assert_eq!(
        (status(&s, "g1"), status(&s, "g2"), status(&s, "g3")),
        ("stopped".into(), "running".into(), "missing".into())
    );
    assert_eq!(
        row(&s, "g2")["canvas"],
        json!({"url": "http://127.0.0.1:8100/canvas_page/file/index.html", "port": 8100})
    );
    assert_eq!(row(&s, "g2")["pid"], json!(7));
    assert_eq!(row(&s, "g1")["canvas"], Value::Null);
}

#[tokio::test]
async fn an_empty_registry_is_listed_empty() {
    let t = t();
    let s = t.look().await;
    assert_eq!(s["listed"], json!(true));
    assert_eq!(s["rows"], json!([]));
    assert_eq!(t.os.asked("registry"), 1);
}

#[tokio::test]
async fn start_spawns_and_shows_starting_then_running() {
    let t = t();
    t.os.set_registry(vec![entry("g1", "/w/a", None, None)]);
    t.stopped("/w/a");
    t.look().await;
    let s = t.runner.apply(Event::Start {
        id: "g1".into(),
        now_ms: 0,
    });
    assert_eq!(status(&s, "g1"), "starting");
    let s = t.settle().await;
    assert_eq!(t.os.spawned(), ["/w/a"]);
    assert_eq!(status(&s, "g1"), "starting", "still within its grace");
    // it comes up and writes its registry entry
    t.os.set_registry(vec![entry("g1", "/w/a", Some(9), Some(8200))]);
    t.running("/w/a", 9, 8200, "g1");
    let s = t.look().await;
    assert_eq!(status(&s, "g1"), "running");
    assert_eq!(s["problem"], Value::Null);
}

#[tokio::test]
async fn a_failed_spawn_is_the_problem() {
    let t = t();
    t.os.set_registry(vec![entry("g1", "/w/a", None, None)]);
    t.stopped("/w/a");
    t.os.set_spawn("/w/a", Err("no login environment: SHELL is not set".into()));
    t.look().await;
    t.runner.apply(Event::Start {
        id: "g1".into(),
        now_ms: 0,
    });
    let s = t.settle().await;
    assert_eq!(
        s["problem"],
        json!("Could not start a: no login environment: SHELL is not set")
    );
    assert_eq!(status(&s, "g1"), "stopped");
}

#[tokio::test]
async fn stop_signals_the_lock_holder_then_stopped() {
    let t = t();
    t.os.set_registry(vec![entry("g2", "/w/b", Some(7), Some(8100))]);
    t.running("/w/b", 7, 8100, "g2");
    t.look().await;
    t.runner.apply(Event::Stop { id: "g2".into() });
    let s = t.settle().await;
    assert_eq!(t.os.terminated(), ["/w/b"]);
    assert_eq!(status(&s, "g2"), "stopping");
    t.stopped("/w/b");
    let s = t.look().await;
    assert_eq!(status(&s, "g2"), "stopped");
}

#[tokio::test]
async fn a_stop_that_cannot_be_sent_is_the_problem() {
    let t = t();
    t.os.set_registry(vec![entry("g2", "/w/b", Some(7), Some(8100))]);
    t.running("/w/b", 7, 8100, "g2");
    t.os.set_terminate("/w/b", Err("not yours to stop".into()));
    t.look().await;
    t.runner.apply(Event::Stop { id: "g2".into() });
    let s = t.settle().await;
    assert_eq!(s["problem"], json!("Could not stop b: not yours to stop"));
    assert_eq!(status(&s, "g2"), "running");
}

#[tokio::test]
async fn opening_an_unlisted_folder_starts_it_and_registers_its_canvas() {
    let t = t();
    t.look().await;
    let s = t.runner.apply(Event::Open {
        paths: vec!["/w/new".into()],
        now_ms: 0,
    });
    assert_eq!(row(&s, "adding:/w/new")["status"], json!("starting"));
    assert_eq!(
        (s["pages"].clone(), s["selection"].clone()),
        (json!(["adding:/w/new"]), json!("adding:/w/new"))
    );
    t.settle().await;
    assert_eq!(t.os.spawned(), ["/w/new"]);
    // its canvas comes up: the home asks that canvas's own kernel to register it
    t.running("/w/new", 9, 8200, "g-new");
    let register = Forwarded {
        port: 8200,
        target: "user_store".into(),
        payload: json!({"type": "register_home_folder", "value": true}),
    };
    t.runner.apply(Event::Tick { now_ms: 0 });
    while !t.os.forwarded().contains(&register) {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    // its kernel writes its registry entry as it registers: the next read lists
    // it under its guid, and its page follows
    t.os.set_registry(vec![entry("g-new", "/w/new", Some(9), Some(8200))]);
    let s = t.settle().await;
    assert_eq!(status(&s, "g-new"), "running");
    assert_eq!(
        (s["pages"].clone(), s["selection"].clone()),
        (json!(["g-new"]), json!("g-new"))
    );
}

#[tokio::test]
async fn remove_switches_a_running_entry_off() {
    let t = t();
    t.os.set_registry(vec![entry("g2", "/w/b", Some(7), Some(8100))]);
    t.running("/w/b", 7, 8100, "g2");
    t.look().await;
    t.runner.apply(Event::Select {
        id: Some("g2".into()),
    });
    t.runner.apply(Event::Forget { id: "g2".into() });
    let s = t.settle().await;
    assert_eq!(
        t.os.forwarded(),
        [Forwarded {
            port: 8100,
            target: "user_store".into(),
            payload: json!({"type": "register_home_folder", "value": false})
        }]
    );
    assert_eq!(
        s["pages"],
        json!([]),
        "its page closes once it is forgotten"
    );
}

#[tokio::test]
async fn remove_unstores_a_stopped_entry_through_a_running_one() {
    let t = t();
    t.os.set_registry(vec![
        entry("g1", "/w/a", None, None),
        entry("g2", "/w/b", Some(7), Some(8100)),
    ]);
    t.stopped("/w/a");
    t.running("/w/b", 7, 8100, "g2");
    t.look().await;
    t.runner.apply(Event::Forget { id: "g1".into() });
    t.settle().await;
    assert_eq!(
        t.os.forwarded(),
        [Forwarded {
            port: 8100,
            target: "user_store".into(),
            payload: json!({"type": "unstore", "guid": "g1"})
        }]
    );
}

#[tokio::test]
async fn remove_with_nothing_running_is_the_problem() {
    let t = t();
    t.os.set_registry(vec![entry("g1", "/w/a", None, None)]);
    t.stopped("/w/a");
    t.look().await;
    let s = t.runner.apply(Event::Forget { id: "g1".into() });
    assert!(
        s["problem"]
            .as_str()
            .is_some_and(|p| p.starts_with("Start any fantastic to remove a")),
        "{s}"
    );
    t.settle().await;
    assert!(t.os.forwarded().is_empty());
}

#[tokio::test]
async fn a_tick_during_a_look_is_dropped_and_an_asked_look_runs_after() {
    let t = t();
    t.os.hold_registry();
    t.runner.apply(Event::Tick { now_ms: 0 });
    while t.os.asked("registry") == 0 {
        tokio::task::yield_now().await;
    }
    // a tick during the look is dropped
    t.runner.apply(Event::Tick { now_ms: 0 });
    // a look asked for during it runs once, after it — however often it is asked
    t.runner.look();
    t.runner.look();
    tokio::task::yield_now().await;
    assert_eq!(t.os.asked("registry"), 1);
    t.os.release();
    t.runner.settle().await;
    assert_eq!(t.os.asked("registry"), 2);
}

#[tokio::test]
async fn a_verified_run_is_not_asked_its_guid_twice() {
    let t = t();
    t.os.set_registry(vec![entry("g2", "/w/b", Some(7), Some(8100))]);
    t.running("/w/b", 7, 8100, "g2");
    t.look().await;
    t.look().await;
    let s = t.look().await;
    assert_eq!(status(&s, "g2"), "running");
    assert_eq!(t.os.asked("guid"), 1);
    assert_eq!(
        t.os.asked("canvas_up"),
        3,
        "its canvas is still asked every look"
    );
}

#[tokio::test]
async fn the_state_is_emitted_only_on_change_with_a_rising_seq() {
    let t = t();
    t.os.set_registry(vec![entry("g1", "/w/a", None, None)]);
    t.stopped("/w/a");
    t.look().await;
    let n = t.emitted.lock().unwrap().len();
    assert!(n > 0);
    // nothing changes: nothing is emitted, the seq stays
    let s = t.look().await;
    t.runner.state();
    assert_eq!(t.emitted.lock().unwrap().len(), n);
    assert_eq!(s["seq"], json!(n));
    // a change is emitted, one higher
    t.runner.apply(Event::Select {
        id: Some("g1".into()),
    });
    let emitted = t.emitted.lock().unwrap().clone();
    assert_eq!(emitted.len(), n + 1);
    let seqs: Vec<u64> = emitted.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=(n as u64 + 1)).collect::<Vec<_>>());
    assert!(emitted.iter().all(|e| e["type"] == json!("state")));
}

#[tokio::test]
async fn pages_and_selection_are_kept_only_once_listed_and_on_change() {
    let t = t();
    t.os.set_registry(vec![
        entry("g1", "/w/a", None, None),
        entry("g2", "/w/b", None, None),
    ]);
    t.stopped("/w/a");
    t.stopped("/w/b");
    // at boot: the record's values, restored
    t.runner.set_kept(vec!["g1".into()], Some("g1".into()));
    t.runner.apply(Event::Restore {
        pages: vec!["g1".into()],
        selection: Some("g1".into()),
    });
    assert!(
        t.kept.lock().unwrap().is_empty(),
        "nothing is kept before the first look"
    );
    let s = t.look().await;
    assert_eq!(
        (s["pages"].clone(), s["selection"].clone()),
        (json!(["g1"]), json!("g1"))
    );
    assert!(
        t.kept.lock().unwrap().is_empty(),
        "the restored values are the kept ones"
    );
    t.runner.apply(Event::Select {
        id: Some("g2".into()),
    });
    t.runner.apply(Event::Select {
        id: Some("g2".into()),
    });
    t.runner.apply(Event::Select { id: None });
    assert_eq!(
        *t.kept.lock().unwrap(),
        [
            (
                vec!["g1".to_string(), "g2".to_string()],
                Some("g2".to_string())
            ),
            (vec!["g1".to_string(), "g2".to_string()], None)
        ]
    );
}
