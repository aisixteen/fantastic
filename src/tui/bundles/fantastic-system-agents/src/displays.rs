//! THE DISPLAYS AGENT's bundle: the verbs become intents of `DisplaysVm`, the driver
//! runs it against the `DisplaysOs` port. The compositor's outputs
//! (`wlr-randr`) and the backlight (logind).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use fantastic_system::displays::{Args, Call, Config, DisplaysVm, Effect, Event, ModeArg};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::driver::{self, Driver, Machine, Port, Step};
use crate::sys::{self, DisplaysOs};

pub const HANDLER_MODULE: &str = "displays.tools";
pub const ID: &str = "displays";
pub const VIEW: &str = include_str!("views/displays.html");

/// What the agent's view may call.
pub const VERBS: [&str; 5] = ["reflect", "state", "set_output", "set_brightness", "boot"];

const SENTENCE: &str = "The machine's displays: the compositor's outputs through wlr-randr, and the backlight through logind. Read `state`; `set_output` changes an output's mode, scale, transform, position or power, `set_brightness` the backlight.";

/// The verbs that are intents.
const ACTS: [&str; 2] = ["set_output", "set_brightness"];

impl Machine for DisplaysVm {
    type Event = Event;
    type Effect = Effect;

    fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        DisplaysVm::reduce(self, ev)
    }
    fn state(&self) -> Value {
        serde_json::to_value(DisplaysVm::state(self)).unwrap_or(Value::Null)
    }
    fn start() -> Event {
        Event::Start
    }
    fn tick(now_ms: u64) -> Event {
        Event::Tick { now_ms }
    }
    fn intent(verb: &str, args: &Value) -> Option<Event> {
        let (mode, bad_mode) = match args.get("mode").filter(|v| !v.is_null()) {
            None => (None, false),
            Some(Value::String(s)) => {
                let m = ModeArg::parse(s);
                (m, m.is_none())
            }
            Some(Value::Object(o)) => {
                let n = |k: &str| {
                    o.get(k)
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                };
                let refresh = o
                    .get("refresh_mhz")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .or_else(|| {
                        o.get("refresh")
                            .and_then(Value::as_f64)
                            .map(|hz| (hz * 1000.0).round() as u32)
                    });
                match (n("width"), n("height")) {
                    (Some(width), Some(height)) => (
                        Some(ModeArg {
                            width,
                            height,
                            refresh_mhz: refresh,
                        }),
                        false,
                    ),
                    _ => (None, true),
                }
            }
            Some(_) => (None, true),
        };
        let position = args.get("position").and_then(|p| {
            let p = p.as_array()?;
            let at = |i: usize| p.get(i)?.as_i64().and_then(|n| i32::try_from(n).ok());
            Some((at(0)?, at(1)?))
        });
        ACTS.contains(&verb).then(|| Event::Intent {
            verb: verb.into(),
            args: Args {
                name: args.get("name").and_then(Value::as_str).map(str::to_string),
                mode,
                bad_mode,
                scale: args.get("scale").and_then(Value::as_f64),
                transform: args
                    .get("transform")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                position,
                enabled: args.get("enabled").and_then(Value::as_bool),
                value: args.get("value").and_then(Value::as_f64),
            },
        })
    }
}

/// The VM's effects, run on the OS port.
struct DisplaysPort(Arc<dyn DisplaysOs>);

#[async_trait]
impl Port<DisplaysVm> for DisplaysPort {
    async fn run(&self, effect: Effect) -> Step<Event> {
        match effect {
            Effect::Read => match self.0.read().await {
                Ok(r) => Step::Events(vec![Event::Read(r)]),
                Err(e) => Step::Events(vec![Event::Unavailable {
                    why: e.message().into(),
                }]),
            },
            Effect::Call(c) => {
                let name = call_name(&c);
                match self.0.call(c).await {
                    Ok(()) => Step::Events(vec![Event::Changed]),
                    Err(e) => Step::Refused {
                        reason: e.reason().into(),
                        message: format!("{name}: {}", e.message()),
                    },
                }
            }
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
        Call::SetOutput { .. } => "set_output",
        Call::SetBrightness { .. } => "set_brightness",
    }
}

/// Where the OS port comes from.
enum Source {
    /// This machine's.
    Real,
    /// A given one (tests).
    Fixed(Arc<dyn DisplaysOs>),
}

/// A running agent: its driver and the loop's task.
type Running = (Arc<Driver<DisplaysVm>>, TaskId);

pub struct DisplaysBundle {
    source: Source,
    running: Mutex<HashMap<AgentId, Running>>,
}

impl DisplaysBundle {
    /// On this machine's OS port.
    pub fn new() -> Self {
        DisplaysBundle {
            source: Source::Real,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// On a given port.
    pub fn with_port(port: Arc<dyn DisplaysOs>) -> Self {
        DisplaysBundle {
            source: Source::Fixed(port),
            running: Mutex::new(HashMap::new()),
        }
    }

    fn driver(&self, id: &AgentId) -> Option<Arc<Driver<DisplaysVm>>> {
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
            Source::Real => sys::displays().await,
            Source::Fixed(p) => Arc::clone(p),
        };
        let (drv, task) = Driver::start(
            kernel,
            id,
            "displays_loop",
            DisplaysVm::new(Config::default()),
            Arc::new(DisplaysPort(port)),
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
            None => Machine::state(&DisplaysVm::new(Config::default())),
        }
    }
}

impl Default for DisplaysBundle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Bundle for DisplaysBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn verb_docs(&self) -> Value {
        json!({
            "reflect": "No args. {id, sentence, available, why?}: `available` is false, with `why`, when wlr-randr is not installed or there is no Wayland display (\"no Wayland display at /run/user/<uid>/wayland-0\").",
            "state": "No args. {available, why?, outputs: [{name, enabled, make, model, mode: {width, height, refresh_mhz}|null (off), modes: [{width, height, refresh_mhz, preferred, current}], scale, transform (normal|90|180|270|flipped|flipped-90|flipped-180|flipped-270), position: [x, y]}], backlight: {name, value (0.0..1.0)}|null (none)}. Read every 5 s and after each change.",
            "set_output": "args: name (an output), mode? (\"WxH\", \"WxH@R\" or {width, height, refresh_mhz?}; one of the output's `modes`), scale? (number above 0), transform?, position? ([x, y]), enabled? (bool). At least one of the five. Refuses turning off the last enabled output. Refused: bad_arg, not_found (no such output), unsupported (the last output), unavailable, failed. Returns the state.",
            "set_brightness": "args: value (number, clamped to 0.0..1.0). Sets the backlight through logind. Refused: bad_arg, unsupported (no backlight), unavailable, denied, failed. Returns the state.",
            "boot": "Kernel lifecycle — starts the agent's loop (reads wlr-randr and the backlight every 5 s). Returns {running}.",
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
                    &format!("{verb}: the displays agent has not booted"),
                ),
            },
            _ => {
                let mut r = fantastic_kernel::refusal(
                    fantastic_kernel::refusal::UNHANDLED,
                    format!("displays: unknown type {verb:?}"),
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
