//! The unit suite's on-device model: an [`FmEngine`] that answers from a script,
//! so nothing in the default suite needs FoundationModels, Apple Intelligence or
//! macOS 26. The `swift/Tests/FantasticFMTests/FakeEngine.swift`, step for step:
//! an unavailable engine ends every stream with FMCore's refusal; a pass streams
//! its script's texts, then ends (`failure` / `hang` as configured).

use crate::engine::{
    unavailable_message, FmAvailability, FmEngine, FmEvent, FmRequest, FmStream, FM_CANCELLED,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// One scripted step of a pass.
#[derive(Clone, Debug)]
pub(crate) enum Step {
    /// A text delta.
    Text(&'static str),
    /// The model's count of the prompt, reported on the pass's end.
    PromptTokens(i64),
}

pub(crate) struct FakeEngine {
    availability: Mutex<FmAvailability>,
    context_size: i64,
    script: Mutex<VecDeque<Vec<Step>>>,
    /// What a pass streams once the script is spent.
    reply: Vec<Step>,
    /// The pass ends with this error after its texts, when set.
    failure: Option<String>,
    /// After its texts, a pass waits until it is cancelled.
    hang: bool,
    requests: Mutex<Vec<FmRequest>>,
    cancels: Arc<AtomicUsize>,
}

impl FakeEngine {
    pub(crate) fn new() -> Self {
        Self {
            availability: Mutex::new(FmAvailability::Available),
            context_size: 32768,
            script: Mutex::new(VecDeque::new()),
            reply: vec![Step::Text("ok "), Step::Text("<call name=\"done\"></call>")],
            failure: None,
            hang: false,
            requests: Mutex::new(Vec::new()),
            cancels: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn context_size(mut self, n: i64) -> Self {
        self.context_size = n;
        self
    }

    pub(crate) fn unavailable(self, reason: &str) -> Self {
        self.set_availability(FmAvailability::Unavailable(reason.to_string()));
        self
    }

    pub(crate) fn script(self, passes: Vec<Vec<Step>>) -> Self {
        *self.script.lock().unwrap() = passes.into();
        self
    }

    pub(crate) fn failure(mut self, e: &str) -> Self {
        self.failure = Some(e.to_string());
        self
    }

    pub(crate) fn hang(mut self) -> Self {
        self.hang = true;
        self
    }

    pub(crate) fn set_availability(&self, a: FmAvailability) {
        *self.availability.lock().unwrap() = a;
    }

    /// Every request a stream was opened with, in order.
    pub(crate) fn requests(&self) -> Vec<FmRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// How many live streams were cancelled.
    pub(crate) fn cancels(&self) -> usize {
        self.cancels.load(Ordering::SeqCst)
    }
}

impl FmEngine for FakeEngine {
    fn availability(&self) -> FmAvailability {
        self.availability.lock().unwrap().clone()
    }

    fn context_size(&self) -> i64 {
        self.context_size
    }

    fn stream(&self, request: &FmRequest) -> FmStream {
        self.requests.lock().unwrap().push(request.clone());
        let steps = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.reply.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        let cancels = Arc::clone(&self.cancels);
        // FMCore's preflight: an unavailable model refuses before any text.
        if let FmAvailability::Unavailable(r) = self.availability() {
            let _ = tx.send(FmEvent::Done {
                error: Some(unavailable_message(&r)),
                prompt_tokens: -1,
            });
            return FmStream::new(rx, move || {
                cancels.fetch_add(1, Ordering::SeqCst);
            });
        }
        let mut prompt_tokens = -1;
        for s in steps {
            match s {
                Step::Text(t) => {
                    let _ = tx.send(FmEvent::Text(t.to_string()));
                }
                Step::PromptTokens(n) => prompt_tokens = n,
            }
        }
        if self.hang {
            // The sender lives in the cancel: the stream ends only when cancelled,
            // with the bridge's `cancelled`.
            return FmStream::new(rx, move || {
                cancels.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(FmEvent::Done {
                    error: Some(FM_CANCELLED.to_string()),
                    prompt_tokens: -1,
                });
            });
        }
        let _ = tx.send(FmEvent::Done {
            error: self.failure.clone(),
            prompt_tokens,
        });
        FmStream::new(rx, move || {
            cancels.fetch_add(1, Ordering::SeqCst);
        })
    }
}
