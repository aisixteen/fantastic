//! THE NOTIFICATIONS AGENT's bundle: the verbs become intents of
//! `NotificationsVm`, the driver runs it against the `NotificationsOs` port.
//! The agent is the session's notification server
//! (`org.freedesktop.Notifications`): what clients send lands in `state`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use fantastic_system::notifications::{Args, Call, Config, Effect, Event, NotificationsVm};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::driver::{self, Driver, Machine, Port, Step};
use crate::sys::{self, NotificationsOs};

pub const HANDLER_MODULE: &str = "notifications.tools";
pub const ID: &str = "notifications";
pub const VIEW: &str = include_str!("views/notifications.html");

/// What the agent's view may call.
pub const VERBS: [&str; 7] = [
    "reflect",
    "state",
    "notify",
    "dismiss",
    "dismiss_all",
    "invoke",
    "set_dnd",
];

const SENTENCE: &str = "The session's notification server: this agent owns org.freedesktop.Notifications, so every notification a program sends lands in `state`. `notify` posts one, `dismiss` and `dismiss_all` close them, `invoke` triggers an action, `set_dnd` marks new ones quiet.";

/// The verbs that are intents.
const ACTS: [&str; 5] = ["notify", "dismiss", "dismiss_all", "invoke", "set_dnd"];

impl Machine for NotificationsVm {
    type Event = Event;
    type Effect = Effect;

    fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        NotificationsVm::reduce(self, ev)
    }
    fn state(&self) -> Value {
        serde_json::to_value(NotificationsVm::state(self)).unwrap_or(Value::Null)
    }
    fn start() -> Event {
        Event::Start
    }
    fn tick(now_ms: u64) -> Event {
        Event::Tick { now_ms }
    }
    fn intent(verb: &str, args: &Value) -> Option<Event> {
        let text = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        // an id that is no u32 names no notification
        let id = args.get("id").filter(|v| !v.is_null()).map(|v| {
            v.as_u64()
                .and_then(|i| u32::try_from(i).ok())
                .unwrap_or(u32::MAX)
        });
        ACTS.contains(&verb).then(|| Event::Intent {
            verb: verb.into(),
            args: Args {
                id,
                action: text("action"),
                summary: text("summary"),
                body: text("body"),
                urgency: text("urgency"),
                on: args.get("on").and_then(Value::as_bool),
                now_ms: driver::now_ms(),
            },
        })
    }
}

/// The VM's effects, run on the OS port.
struct NotificationsPort(Arc<dyn NotificationsOs>);

#[async_trait]
impl Port<NotificationsVm> for NotificationsPort {
    async fn run(&self, effect: Effect) -> Step<Event> {
        match effect {
            Effect::Call(Call::Request) => match self.0.request().await {
                Ok(o) => Step::Events(vec![Event::Ownership(o)]),
                Err(e) => Step::Events(vec![Event::Unavailable {
                    why: e.message().into(),
                }]),
            },
            Effect::Call(c) => match self.0.call(c.clone()).await {
                Ok(()) => Step::Events(vec![]),
                Err(e) => Step::Refused {
                    reason: e.reason().into(),
                    message: format!("{}: {}", call_name(&c), e.message()),
                },
            },
            Effect::Refuse {
                reason, message, ..
            } => Step::Refused { reason, message },
        }
    }
    fn changes(&self) -> mpsc::Receiver<Event> {
        self.0.changes()
    }
}

fn call_name(c: &Call) -> &'static str {
    match c {
        Call::Request => "request",
        Call::Closed { .. } => "NotificationClosed",
        Call::ActionInvoked { .. } => "ActionInvoked",
        Call::Reply { .. } => "Notify",
    }
}

/// Where the OS port comes from.
enum Source {
    /// This machine's.
    Real,
    /// A given one (tests).
    Fixed(Arc<dyn NotificationsOs>),
}

/// A running agent: its driver and the loop's task.
type Running = (Arc<Driver<NotificationsVm>>, TaskId);

pub struct NotificationsBundle {
    source: Source,
    running: Mutex<HashMap<AgentId, Running>>,
}

impl NotificationsBundle {
    /// On this machine's OS port.
    pub fn new() -> Self {
        NotificationsBundle {
            source: Source::Real,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// On a given port.
    pub fn with_port(port: Arc<dyn NotificationsOs>) -> Self {
        NotificationsBundle {
            source: Source::Fixed(port),
            running: Mutex::new(HashMap::new()),
        }
    }

    fn driver(&self, id: &AgentId) -> Option<Arc<Driver<NotificationsVm>>> {
        self.running
            .lock()
            .expect("running poisoned")
            .get(id)
            .map(|(d, _)| Arc::clone(d))
    }

    async fn boot(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        if self.driver(id).is_some() {
            return json!({"running": true});
        }
        let port = match &self.source {
            Source::Real => sys::notifications().await,
            Source::Fixed(p) => Arc::clone(p),
        };
        let (drv, task) = Driver::start(
            kernel,
            id,
            "notifications_loop",
            NotificationsVm::new(Config::default()),
            Arc::new(NotificationsPort(port)),
        );
        self.running
            .lock()
            .expect("running poisoned")
            .insert(id.clone(), (drv, task));
        json!({"running": true})
    }

    async fn state(&self, id: &AgentId) -> Value {
        match self.driver(id) {
            Some(d) => d.state().await,
            None => Machine::state(&NotificationsVm::new(Config::default())),
        }
    }
}

impl Default for NotificationsBundle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Bundle for NotificationsBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn verb_docs(&self) -> Value {
        json!({
            "reflect": "No args. {id, sentence, available, why?}: `available` is false, with `why`, when the session bus is not reachable or another notification server holds the name.",
            "state": "No args. {available, why?, server (owned|taken: <unique name>|none), items: [{id, app, summary, body, urgency (low|normal|critical), actions: [[key, label]], at_ms, expires_ms|null, category?, desktop_entry?, resident?, quiet?}] (oldest first, at most 50), do_not_disturb}. Expiry runs on a 5 s clock: -1 is 8 s, 0 and critical never.",
            "notify": "args: summary, body?, urgency? (low|normal|critical). Posts a notification as app `fantastic`. Refused: bad_arg, unavailable (the name is not ours). Returns the state.",
            "dismiss": "args: id. Closes it (NotificationClosed, reason 2). Refused: bad_arg, not_found, unavailable. Returns the state.",
            "dismiss_all": "No args. Closes every notification (reason 2). Refused: unavailable. Returns the state.",
            "invoke": "args: id, action (a key from the item's `actions`). Emits ActionInvoked, then closes it (reason 2) unless the item is `resident`. Refused: bad_arg, not_found, unavailable. Returns the state.",
            "set_dnd": "args: on (bool). While on, new items are marked `quiet: true`; they still arrive. Refused: bad_arg, unavailable. Returns the state.",
            "boot": "Kernel lifecycle — starts the agent's loop (takes the name on the session bus without queueing, serves it). Returns {running}.",
        })
    }

    fn emits_doc(&self) -> Option<Value> {
        Some(
            json!({"state": "{type: \"state\", state}: to the agent's watchers after every change of the state, at most one per 100 ms, the latest."}),
        )
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "boot" => self.boot(agent_id, kernel).await,
            "state" => self.state(agent_id).await,
            "reflect" => {
                let s = self.state(agent_id).await;
                let mut r =
                    json!({"id": agent_id.0, "sentence": SENTENCE, "available": s["available"]});
                if let Some(why) = s.get("why").filter(|w| !w.is_null()) {
                    r["why"] = why.clone();
                }
                r
            }
            v if ACTS.contains(&v) => match self.driver(agent_id) {
                Some(d) => d.intent(verb, payload).await,
                None => driver::refusal(
                    "unavailable",
                    &format!("{verb}: the notifications agent has not booted"),
                ),
            },
            _ => {
                let mut r = fantastic_kernel::refusal(
                    fantastic_kernel::refusal::UNHANDLED,
                    format!("notifications: unknown type {verb:?}"),
                );
                fantastic_kernel::refusal::with(&mut r, "verb", json!(verb));
                r
            }
        };
        Ok(Some(reply))
    }

    /// The loop dies with the agent.
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let gone = self
            .running
            .lock()
            .expect("running poisoned")
            .remove(agent_id);
        if let Some((_, task)) = gone {
            kernel.cancel_task(&task).await;
        }
        Ok(())
    }
}
