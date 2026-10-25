//! THE DRIVER: one task per agent runs a pure ViewModel against its OS port.
//! Events in (OS reads, OS changes, intents, ticks), effects run through the
//! port, and after a `reduce` that changed the state a `{type: "state", state}`
//! goes to the agent itself (`kernel.emit`), so its watchers see it. At most one
//! such emit per 100 ms per agent, always the latest state.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::{AgentId, Kernel, TaskId};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::Instant;

/// The debounce of `state` emits.
pub const DEBOUNCE: Duration = Duration::from_millis(100);
/// How often the ViewModel is told the time.
pub const TICK: Duration = Duration::from_secs(5);

/// A ViewModel, as the driver sees it.
pub trait Machine: Send + 'static {
    type Event: Send + 'static;
    type Effect: Send + 'static;
    /// The only mutation.
    fn reduce(&mut self, ev: Self::Event) -> Vec<Self::Effect>;
    /// What the views draw: the `state` reply.
    fn state(&self) -> Value;
    /// The first event of a boot: read everything.
    fn start() -> Self::Event;
    fn tick(now_ms: u64) -> Self::Event;
    /// A verb as an intent event; `None` when the agent has no such verb.
    fn intent(verb: &str, args: &Value) -> Option<Self::Event>;
}

/// What running one effect came to.
pub enum Step<E> {
    /// Events to feed back (a read's answer, a failure as `Unavailable`).
    Events(Vec<E>),
    /// The effect is a refusal of the verb in flight.
    Refused { reason: String, message: String },
}

/// A ViewModel's OS side.
#[async_trait]
pub trait Port<M: Machine>: Send + Sync + 'static {
    async fn run(&self, effect: M::Effect) -> Step<M::Event>;
    /// OS-side changes, until the port is dropped.
    fn changes(&self) -> mpsc::Receiver<M::Event>;
}

struct Inner<M: Machine> {
    vm: M,
    port: Arc<dyn Port<M>>,
}

/// One running agent.
pub struct Driver<M: Machine> {
    inner: Mutex<Inner<M>>,
    state: watch::Sender<Value>,
}

/// The refusal a reply carries: `{error, reason}`.
pub fn refusal(reason: &str, message: &str) -> Value {
    json!({"error": message, "reason": reason})
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl<M: Machine> Driver<M> {
    /// Start the agent's loop (`kernel.spawn_task(id, name, …)`): the returned
    /// task id is what `on_delete` cancels.
    pub fn start(
        kernel: &Arc<Kernel>,
        id: &AgentId,
        task_name: &str,
        vm: M,
        port: Arc<dyn Port<M>>,
    ) -> (Arc<Self>, TaskId) {
        let (state, _) = watch::channel(vm.state());
        let drv = Arc::new(Driver {
            inner: Mutex::new(Inner { vm, port }),
            state,
        });
        let changes = drv.inner.try_lock().expect("fresh").port.changes();
        let task = kernel.spawn_task(
            id,
            task_name,
            run_loop(Arc::clone(&drv), Arc::clone(kernel), id.clone(), changes),
        );
        (drv, task)
    }

    /// The state object, as the `state` verb answers it.
    pub async fn state(&self) -> Value {
        self.inner.lock().await.vm.state()
    }

    /// A verb: the state after its effects ran, or the refusal.
    pub async fn intent(&self, verb: &str, args: &Value) -> Value {
        let Some(ev) = M::intent(verb, args) else {
            return refusal("unhandled", &format!("unknown verb {verb:?}"));
        };
        let refusal_of = self.feed(ev).await;
        match refusal_of {
            Some((reason, message)) => refusal(&reason, &message),
            None => self.state().await,
        }
    }

    /// Reduce one event and everything it asks for. The first refusal is returned.
    async fn feed(&self, ev: M::Event) -> Option<(String, String)> {
        let mut inner = self.inner.lock().await;
        let port = Arc::clone(&inner.port);
        let mut queue = std::collections::VecDeque::from([ev]);
        let mut refused = None;
        while let Some(ev) = queue.pop_front() {
            for effect in inner.vm.reduce(ev) {
                match port.run(effect).await {
                    Step::Events(v) => queue.extend(v),
                    Step::Refused { reason, message } => {
                        refused.get_or_insert((reason, message));
                    }
                }
            }
        }
        let now = inner.vm.state();
        drop(inner);
        self.state.send_if_modified(|v| {
            if *v == now {
                false
            } else {
                *v = now;
                true
            }
        });
        refused
    }
}

async fn run_loop<M: Machine>(
    drv: Arc<Driver<M>>,
    kernel: Arc<Kernel>,
    id: AgentId,
    mut changes: mpsc::Receiver<M::Event>,
) {
    let mut seen = drv.state.subscribe();
    seen.mark_unchanged();
    drv.feed(M::start()).await;
    let mut tick = tokio::time::interval(TICK);
    tick.tick().await;
    let (mut open, mut due): (bool, Option<Instant>) = (true, None);
    loop {
        tokio::select! {
            ev = changes.recv(), if open => match ev {
                Some(ev) => { drv.feed(ev).await; }
                None => open = false,
            },
            _ = tick.tick() => { drv.feed(M::tick(now_ms())).await; }
            _ = seen.changed() => {
                // marked seen here, so the wake is not repeated; the emit reads the latest
                seen.borrow_and_update();
                if due.is_none() {
                    due = Some(Instant::now() + DEBOUNCE);
                }
            }
            _ = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => {
                due = None;
                let state = seen.borrow().clone();
                kernel.emit(&id, json!({"type": "state", "state": state})).await;
            }
        }
    }
}
