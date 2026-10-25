//! The notifications agent against a real in-memory kernel, on a fake OS port:
//! verb -> port call -> reply, a client's `Notify` -> `state` event to a
//! watcher, no session bus -> `available: false`, `on_delete` stops the loop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system::notifications::{Call, Event, Notification, Ownership};
use fantastic_system_agents::notifications::{NotificationsBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::{NotificationsOs, OsError};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// The fake: what asking for the name answers, what it was called with, and
/// the channel the clients' calls ride.
struct Fake {
    request: Mutex<Result<Ownership, OsError>>,
    calls: Mutex<Vec<Call>>,
    tx: mpsc::Sender<Event>,
    rx: Mutex<Option<mpsc::Receiver<Event>>>,
}

impl Fake {
    fn new(request: Result<Ownership, OsError>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(16);
        Arc::new(Fake {
            request: Mutex::new(request),
            calls: Mutex::new(vec![]),
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }
}

#[async_trait]
impl NotificationsOs for Fake {
    async fn request(&self) -> Result<Ownership, OsError> {
        self.request.lock().unwrap().clone()
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

fn client_note(summary: &str) -> Notification {
    Notification {
        app: "notify-send".into(),
        replaces_id: 0,
        icon: String::new(),
        summary: summary.into(),
        body: String::new(),
        actions: vec![],
        hints: Default::default(),
        expire_timeout: -1,
        now_ms: 1,
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
        NotificationsBundle::with_port(Arc::clone(fake) as Arc<dyn NotificationsOs>),
    );
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "notifications", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");
    k
}

/// Wait until `notifications` reports `available` as asked.
async fn settled(k: &Arc<Kernel>, available: bool) -> Value {
    for _ in 0..200 {
        let s = op(k, "notifications", json!({"type": "state"})).await;
        if s["available"] == json!(available) && s["why"] != json!("not read yet") {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("notifications never settled to available={available}");
}

/// A watcher: an inbox on `notifications`.
fn watcher(k: &Arc<Kernel>) -> fantastic_kernel::inbox::InboxReceiver {
    let w = AgentId::from("watcher");
    let rx = k.ensure_inbox(&w);
    let k2 = Arc::clone(k);
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { k2.watch(&AgentId::from("notifications"), w).await })
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
    let fake = Fake::new(Ok(Ownership::Owned));
    let k = kernel(&fake).await;
    let s = settled(&k, true).await;
    assert_eq!(s["server"], "owned");
    let r = op(
        &k,
        "notifications",
        json!({"type": "notify", "summary": "build done", "body": "green"}),
    )
    .await;
    assert_eq!(
        r["items"][0]["summary"], "build done",
        "the reply is the state: {r}"
    );
    assert_eq!(r["items"][0]["app"], "fantastic");
    let id = r["items"][0]["id"].as_u64().unwrap();
    let r = op(&k, "notifications", json!({"type": "dismiss", "id": id})).await;
    assert_eq!(r["items"].as_array().unwrap().len(), 0, "{r}");
    assert!(fake.calls.lock().unwrap().contains(&Call::Closed {
        id: id as u32,
        reason: 2
    }));
    let r = op(&k, "notifications", json!({"type": "dismiss", "id": 77})).await;
    assert_eq!(r["reason"], "not_found", "{r}");
    let r = op(&k, "notifications", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["id"].clone(), r["available"].clone()),
        (json!("notifications"), json!(true))
    );
    assert!(r["sentence"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_change_reaches_a_watcher_as_one_latest_state() {
    let fake = Fake::new(Ok(Ownership::Owned));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    let mut rx = watcher(&k);
    // the boot's own emit may still be coming: drain it
    while next_state(&mut rx, Duration::from_millis(400))
        .await
        .is_some()
    {}
    // five Notify calls 20 ms apart (inside the 100 ms window): at most two
    // emits, the last holding all five; each is answered with its id
    for n in 1..=5 {
        fake.tx
            .send(Event::Notified(client_note(&format!("n{n}"))))
            .await
            .unwrap();
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
    assert_eq!(got.last().unwrap()["items"].as_array().unwrap().len(), 5);
    let replies: Vec<u32> = fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter_map(|c| match c {
            Call::Reply { id } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(replies, vec![1, 2, 3, 4, 5]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_absent_service_is_unavailable_and_refuses_verbs() {
    let why = "the session bus is not reachable";
    let fake = Fake::new(Err(OsError::Unavailable(why.into())));
    let k = kernel(&fake).await;
    let s = settled(&k, false).await;
    assert_eq!(s["why"], why);
    let r = op(&k, "notifications", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["available"].clone(), r["why"].clone()),
        (json!(false), json!(why))
    );
    let r = op(
        &k,
        "notifications",
        json!({"type": "notify", "summary": "x"}),
    )
    .await;
    assert_eq!(r["reason"], "unavailable", "{r}");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_held_by_another_server_is_taken() {
    let fake = Fake::new(Ok(Ownership::Taken { by: ":1.42".into() }));
    let k = kernel(&fake).await;
    let s = settled(&k, false).await;
    assert_eq!(s["server"], "taken: :1.42");
    let r = op(
        &k,
        "notifications",
        json!({"type": "notify", "summary": "x"}),
    )
    .await;
    assert_eq!(r["reason"], "unavailable", "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_agent_stops_its_loop() {
    let fake = Fake::new(Ok(Ownership::Owned));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    assert!(!fake.tx.is_closed(), "the loop holds the changes receiver");
    let root = k.root().unwrap().id.0.clone();
    let r = op(
        &k,
        &root,
        json!({"type": "delete_agent", "id": "notifications"}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    for _ in 0..100 {
        if fake.tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the loop kept running after on_delete");
}
