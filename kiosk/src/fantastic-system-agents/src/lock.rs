//! THE LOCK AGENT's bundle: the verbs become intents of `LockVm`, the driver
//! runs it against the `LockOs` port. The kiosk's lock screen: the Home page
//! draws it while `state.locked`; this agent keeps the PIN (Argon2id, in
//! `<home>/lock.yaml`) and follows logind's `Lock` and `Unlock`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use fantastic_system::lock::{Args, Call, Config, Effect, Event, LockVm, Pin};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::driver::{self, Driver, Machine, Port, Step};
use crate::sys::{self, LockOs};

pub const HANDLER_MODULE: &str = "lock.tools";
pub const ID: &str = "lock";
pub const VIEW: &str = include_str!("views/lock.html");

/// What the agent's view may call.
pub const VERBS: [&str; 9] = [
    "reflect",
    "state",
    "lock",
    "unlock",
    "set_pin",
    "clear_pin",
    "set_auto_lock",
    "activity",
    "boot",
];

const SENTENCE: &str = "The kiosk's lock screen, a UI lock and not a security boundary. The Home page covers the screen while `state.locked`. `lock` locks (a PIN must be set), `unlock {pin}` opens, `set_pin {old?, new}` and `clear_pin {pin}` manage the PIN (4 to 12 digits), `set_auto_lock {minutes}` sets the idle lock (0 is off), `activity` is the Home page's input signal. Only `user` may call it.";

/// The verbs that are intents.
const ACTS: [&str; 6] = [
    "lock",
    "unlock",
    "set_pin",
    "clear_pin",
    "set_auto_lock",
    "activity",
];

impl Machine for LockVm {
    type Event = Event;
    type Effect = Effect;

    fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        LockVm::reduce(self, ev)
    }
    fn state(&self) -> Value {
        serde_json::to_value(LockVm::state(self)).unwrap_or(Value::Null)
    }
    fn start() -> Event {
        Event::Start
    }
    fn tick(now_ms: u64) -> Event {
        Event::Tick { now_ms }
    }
    fn intent(verb: &str, args: &Value) -> Option<Event> {
        if verb == "activity" {
            return Some(Event::Activity {
                now_ms: driver::now_ms(),
            });
        }
        // a PIN given as a number loses its leading zeros: only text counts
        let pin = |k: &str| args.get(k).and_then(Value::as_str).map(Pin::new);
        let minutes = args.get("minutes").filter(|v| !v.is_null());
        let parsed = minutes
            .and_then(|v| v.as_u64())
            .and_then(|m| u32::try_from(m).ok());
        ACTS.contains(&verb).then(|| Event::Intent {
            verb: verb.into(),
            args: Args {
                pin: pin("pin"),
                old: pin("old"),
                new: pin("new"),
                minutes: parsed,
                bad_minutes: minutes.is_some() && parsed.is_none(),
                now_ms: driver::now_ms(),
            },
        })
    }
}

/// The VM's effects, run on the OS port.
struct LockPort(Arc<dyn LockOs>);

#[async_trait]
impl Port<LockVm> for LockPort {
    async fn run(&self, effect: Effect) -> Step<Event> {
        match effect {
            Effect::Read => match self.0.read().await {
                Ok(r) => Step::Events(vec![Event::Read(r)]),
                Err(e) => Step::Events(vec![Event::Unavailable {
                    why: e.message().into(),
                }]),
            },
            Effect::CheckPin { pin } => match self.0.check_pin(pin).await {
                Ok(ok) => Step::Events(vec![Event::PinChecked { ok }]),
                Err(e) => Step::Events(vec![Event::PinError {
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
        Call::SetLockedHint { .. } => "SetLockedHint",
        Call::HoldInhibitor { .. } => "Inhibit",
        Call::Persist { .. } => "lock.yaml",
    }
}

/// Where the OS port comes from.
enum Source {
    /// This machine's.
    Real,
    /// A given one (tests).
    Fixed(Arc<dyn LockOs>),
}

/// A running agent: its driver and the loop's task.
type Running = (Arc<Driver<LockVm>>, TaskId);

pub struct LockBundle {
    source: Source,
    running: Mutex<HashMap<AgentId, Running>>,
}

impl LockBundle {
    /// On this machine's OS port.
    pub fn new() -> Self {
        LockBundle {
            source: Source::Real,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// On a given port.
    pub fn with_port(port: Arc<dyn LockOs>) -> Self {
        LockBundle {
            source: Source::Fixed(port),
            running: Mutex::new(HashMap::new()),
        }
    }

    fn driver(&self, id: &AgentId) -> Option<Arc<Driver<LockVm>>> {
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
            Source::Real => sys::lock().await,
            Source::Fixed(p) => Arc::clone(p),
        };
        let (drv, task) = Driver::start(
            kernel,
            id,
            "lock_loop",
            LockVm::new(Config::default()),
            Arc::new(LockPort(port)),
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
            None => Machine::state(&LockVm::new(Config::default())),
        }
    }
}

impl Default for LockBundle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Bundle for LockBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn verb_docs(&self) -> Value {
        json!({
            "reflect": "No args. {id, sentence, available, why?}: `available` is false, with `why`, when logind is not on the system bus or the user has no display session.",
            "state": "No args. {available, why?, locked, has_pin, auto_lock_min (0 = off), failures (wrong PINs in a row), retry_at_ms|null (the epoch ms before which no PIN is looked at)}. A kiosk UI lock, not a security boundary.",
            "lock": "No args. Locks the screen; the Home page covers itself until `unlock`. Refused: unsupported (no PIN set), unavailable. Returns the state.",
            "unlock": "args: pin (text, 4 to 12 digits). Opens the screen when the PIN is right; no effect when the screen is not locked or no PIN is set. After 5 wrong PINs in a row each further try waits 30 s, doubling to 15 min. Refused: wrong_pin, retry_later, bad_arg, busy, unavailable. Returns the state.",
            "set_pin": "args: new (text, 4 to 12 digits), old? (the current PIN; required when one is set). Refused: bad_arg, wrong_pin, retry_later, busy, unavailable. Returns the state.",
            "clear_pin": "args: pin (the current PIN). Removes the PIN (and opens a locked screen); no effect when none is set. Refused: bad_arg, wrong_pin, retry_later, busy, unavailable. Returns the state.",
            "set_auto_lock": "args: minutes (whole number, 0 = off, at most 1440). The screen locks after that many minutes without `activity`. Refused: bad_arg, unavailable. Returns the state.",
            "activity": "No args. The Home page's input signal, taken at most once per 10 s; it moves the idle lock's deadline. Returns the state.",
            "boot": "Kernel lifecycle — starts the agent's loop (reads <home>/lock.yaml, follows the display session's Lock and Unlock, holds the sleep inhibitor). Returns {running}.",
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
                    &format!("{verb}: the lock agent has not booted"),
                ),
            },
            _ => {
                let mut r = fantastic_kernel::refusal(
                    fantastic_kernel::refusal::UNHANDLED,
                    format!("lock: unknown type {verb:?}"),
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
