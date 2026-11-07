//! The displays agent against a real in-memory kernel, on a fake OS port: verb ->
//! port call -> reply, port change -> `state` event to a watcher, no compositor
//! -> `available: false`, `on_delete` stops the loop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system::displays::{BacklightRead, Call, Event, Read};
use fantastic_system_agents::displays::{DisplaysBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::{DisplaysOs, OsError};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// One enabled output with two modes, and a backlight at `level` of 100.
fn read(level: u32) -> Read {
    Read {
        outputs: json!([{
            "name": "Virtual-1", "enabled": true, "make": "", "model": "",
            "modes": [
                {"width": 1280, "height": 800, "refresh": 60.0, "preferred": true, "current": true},
                {"width": 1024, "height": 768, "refresh": 60.0, "preferred": false, "current": false}
            ],
            "position": {"x": 0, "y": 0}, "transform": "normal", "scale": 1.0
        }]),
        backlights: vec![BacklightRead {
            name: "intel_backlight".into(),
            brightness: level,
            max: 100,
            kind: "raw".into(),
        }],
    }
}

/// The fake: what it reads, what it was called with, and the channel its changes ride.
struct Fake {
    read: Mutex<Result<Read, OsError>>,
    calls: Mutex<Vec<Call>>,
    tx: mpsc::Sender<Event>,
    rx: Mutex<Option<mpsc::Receiver<Event>>>,
}

impl Fake {
    fn new(read: Result<Read, OsError>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(16);
        Arc::new(Fake {
            read: Mutex::new(read),
            calls: Mutex::new(vec![]),
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }
}

#[async_trait]
impl DisplaysOs for Fake {
    async fn read(&self) -> Result<Read, OsError> {
        self.read.lock().unwrap().clone()
    }
    async fn call(&self, c: Call) -> Result<(), OsError> {
        self.calls.lock().unwrap().push(c);
        Ok(())
    }
    fn changes(&self) -> mpsc::Receiver<Event> {
        self.rx
            .lock()
            .unwrap()
            .take()
            .expect("changes() taken once")
    }
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn kernel(fake: &Arc<Fake>) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(
        HANDLER_MODULE,
        DisplaysBundle::with_port(Arc::clone(fake) as Arc<dyn DisplaysOs>),
    );
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "displays", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");
    k
}

/// Wait until `displays` reports `available` as asked.
async fn settled(k: &Arc<Kernel>, available: bool) -> Value {
    for _ in 0..200 {
        let s = op(k, "displays", json!({"type": "state"})).await;
        if s["available"] == json!(available) && s["why"] != json!("not read yet") {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("displays never settled to available={available}");
}

/// A watcher: an inbox on `displays`.
fn watcher(k: &Arc<Kernel>) -> fantastic_kernel::inbox::InboxReceiver {
    let w = AgentId::from("watcher");
    let rx = k.ensure_inbox(&w);
    let k2 = Arc::clone(k);
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { k2.watch(&AgentId::from("displays"), w).await })
    });
    rx
}

async fn next_state(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    within: Duration,
) -> Option<Value> {
    loop {
        match tokio::time::timeout(within, rx.recv()).await {
            Ok(Some(item)) if item.payload["type"] == "state" => {
                return Some(item.payload["state"].clone())
            }
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_verb_calls_the_port_and_replies_the_state() {
    let fake = Fake::new(Ok(read(60)));
    let k = kernel(&fake).await;
    let s = settled(&k, true).await;
    assert_eq!(s["outputs"][0]["mode"]["refresh_mhz"], 60_000);
    assert_eq!(
        s["backlight"],
        json!({"name": "intel_backlight", "value": 0.6})
    );
    let r = op(
        &k,
        "displays",
        json!({"type": "set_output", "name": "Virtual-1", "scale": 1.5, "transform": "90"}),
    )
    .await;
    assert_eq!(r["available"], true, "the reply is the state: {r}");
    assert_eq!(
        fake.calls.lock().unwrap()[0],
        Call::SetOutput {
            name: "Virtual-1".into(),
            argv: [
                "--output",
                "Virtual-1",
                "--scale",
                "1.5",
                "--transform",
                "90"
            ]
            .map(String::from)
            .to_vec()
        }
    );
    let r = op(
        &k,
        "displays",
        json!({"type": "set_brightness", "value": 2.0}),
    )
    .await;
    assert_eq!(r["available"], true, "{r}");
    assert_eq!(
        fake.calls.lock().unwrap()[1],
        Call::SetBrightness {
            name: "intel_backlight".into(),
            brightness: 100
        },
        "clamped to the top"
    );
    // a mode as an object, and as text
    let r = op(
        &k,
        "displays",
        json!({"type": "set_output", "name": "Virtual-1", "mode": {"width": 1024, "height": 768, "refresh_mhz": 60000}}),
    )
    .await;
    assert_eq!(r["available"], true, "{r}");
    let r = op(
        &k,
        "displays",
        json!({"type": "set_output", "name": "Virtual-1", "mode": "1024x768"}),
    )
    .await;
    assert_eq!(r["available"], true, "{r}");
    match &fake.calls.lock().unwrap()[2] {
        Call::SetOutput { argv, .. } => assert_eq!(argv[3], "1024x768@60"),
        other => panic!("{other:?}"),
    }
    let r = op(&k, "displays", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["id"].clone(), r["available"].clone()),
        (json!("displays"), json!(true))
    );
    assert!(r["sentence"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_verb_does_not_call_the_port() {
    let fake = Fake::new(Ok(read(60)));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    let cases = [
        (
            json!({"type": "set_output", "name": "Virtual-1", "enabled": false}),
            "unsupported",
        ),
        (
            json!({"type": "set_output", "name": "nope", "scale": 2}),
            "not_found",
        ),
        (
            json!({"type": "set_output", "name": "Virtual-1", "mode": "wide"}),
            "bad_arg",
        ),
        (
            json!({"type": "set_output", "name": "Virtual-1", "transform": "sideways"}),
            "bad_arg",
        ),
        (
            json!({"type": "set_output", "name": "Virtual-1"}),
            "bad_arg",
        ),
        (json!({"type": "set_brightness"}), "bad_arg"),
    ];
    for (payload, reason) in cases {
        let r = op(&k, "displays", payload.clone()).await;
        assert_eq!(r["reason"], reason, "{payload}: {r}");
    }
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_change_reaches_a_watcher_as_one_latest_state() {
    let fake = Fake::new(Ok(read(60)));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    let mut rx = watcher(&k);
    // the boot's own emit may still be coming: drain it
    while next_state(&mut rx, Duration::from_millis(400))
        .await
        .is_some()
    {}
    // five reads 20 ms apart (inside the 100 ms window): at most two emits,
    // the last carrying the final level
    for level in [10, 20, 30, 40, 50] {
        fake.tx.send(Event::Read(read(level))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut got = Vec::new();
    while let Some(s) = next_state(&mut rx, Duration::from_millis(500)).await {
        got.push(s);
    }
    assert!(
        !got.is_empty() && got.len() <= 2,
        "at most two emits for five changes: {}",
        got.len()
    );
    assert_eq!(
        got.last().unwrap()["backlight"]["value"],
        0.5,
        "the latest state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_absent_service_is_unavailable_and_refuses_verbs() {
    let why = "no Wayland display at /run/user/1000/wayland-0";
    let fake = Fake::new(Err(OsError::Unavailable(why.into())));
    let k = kernel(&fake).await;
    let s = settled(&k, false).await;
    assert_eq!(s["why"], why);
    let r = op(&k, "displays", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["available"].clone(), r["why"].clone()),
        (json!(false), json!(why))
    );
    let r = op(
        &k,
        "displays",
        json!({"type": "set_brightness", "value": 0.5}),
    )
    .await;
    assert_eq!(r["reason"], "unavailable", "{r}");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_agent_stops_its_loop() {
    let fake = Fake::new(Ok(read(60)));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    assert!(!fake.tx.is_closed(), "the loop holds the changes receiver");
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "delete_agent", "id": "displays"})).await;
    assert!(r.get("error").is_none(), "{r}");
    for _ in 0..100 {
        if fake.tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the loop kept running after on_delete");
}
