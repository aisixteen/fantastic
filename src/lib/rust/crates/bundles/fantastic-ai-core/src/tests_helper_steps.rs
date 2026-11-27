//! #795 C2: A HELPER'S MODEL REQUEST IS A STEP ([`crate::model_format::pass::drive`]).
//! The fold summaries, the owed recall's choice and the compact summary are plain code
//! that reads its task answers from the pass's book; one it has not got yet stops it,
//! the step sends that request as one `complete` and ENDS, and the request's settle
//! runs the helper again from its start. Nothing waits for the model.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::model_format::pass::{drive, Pass};
use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderStream};
use async_trait::async_trait;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

/// A model whose every request is held until its gate opens; the n-th answers `answer n`.
struct Held {
    open: tokio::sync::watch::Sender<bool>,
    asked: Mutex<Vec<Vec<Value>>>,
}

impl Held {
    fn new() -> Arc<Self> {
        let (open, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            open,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

#[async_trait]
impl Provider for Held {
    async fn chat(&self, messages: &[Value], _max_tokens: Option<i64>) -> Result<ProviderStream, ProviderError> {
        let n = {
            let mut asked = self.asked.lock().unwrap();
            asked.push(messages.to_vec());
            asked.len()
        };
        let mut rx = self.open.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
        let ev: Vec<Result<ProviderEvent, ProviderError>> = vec![Ok(ProviderEvent::Token(format!("answer {n}")))];
        Ok(Box::pin(futures_util::stream::iter(ev)))
    }
    fn model(&self) -> String {
        "held-1".to_string()
    }
}

/// The helper asks two task requests, one after the other. Its step ends with the first
/// request out (the model still holds it); the settle runs the helper again, which reads
/// the first answer from the book — never asking it twice — and stops at the second; the
/// second's settle runs it to its end, and `next` gets both answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_helpers_task_request_is_a_step_and_its_settle_runs_the_helper_again() {
    let kernel = Arc::new(Kernel::new());
    let provider = Held::new();
    let pass = Pass::for_test(Arc::clone(&provider) as Arc<dyn Provider>);
    let runs = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&runs);
    let helper = move |p: &Pass, _: &mut tokio::sync::oneshot::Sender<(Option<String>, Option<String>)>| {
        counted.fetch_add(1, Ordering::SeqCst);
        let one = p.task(&[json!({"role": "user", "content": "one"})], None)?;
        let two = p.task(&[json!({"role": "user", "content": "two"})], None)?;
        Ok((one.map(|t| t.text), two.map(|t| t.text)))
    };
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    tokio::time::timeout(
        Duration::from_secs(2),
        drive(kernel, AgentId::from("helper_steps"), pass, tx, helper, |tx, _pass, got| async move {
            let _ = tx.send(got);
        }),
    )
    .await
    .expect("the step waited for the model");
    assert!(rx.try_recv().is_err(), "the helper ended before its request settled");
    for _ in 0..100 {
        if provider.asked() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(provider.asked(), 1, "the first request is out, and only it");
    let _ = provider.open.send(true);
    let got = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("the helper never ran to its end")
        .expect("next ran");
    assert_eq!(got, (Some("answer 1".to_string()), Some("answer 2".to_string())));
    assert_eq!(provider.asked(), 2, "an answer read from the book is never asked again");
    assert_eq!(runs.load(Ordering::SeqCst), 3, "the helper ran once, then once per settle");
}

/// A helper that asks nothing runs once and `next` gets its result at once: no request.
#[tokio::test]
async fn a_helper_that_asks_nothing_ends_in_its_first_step() {
    let kernel = Arc::new(Kernel::new());
    let provider = Held::new();
    let pass = Pass::for_test(Arc::clone(&provider) as Arc<dyn Provider>);
    let (tx, rx) = tokio::sync::oneshot::channel();
    drive(kernel, AgentId::from("helper_steps_none"), pass, tx, |_p: &Pass, _: &mut _| Ok(7), |tx, _pass, n| async move {
        let _ = tx.send(n);
    })
    .await;
    assert_eq!(rx.await.expect("next ran"), 7);
    assert_eq!(provider.asked(), 0);
}
