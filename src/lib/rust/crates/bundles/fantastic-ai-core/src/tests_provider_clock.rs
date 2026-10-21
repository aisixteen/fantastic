//! The read clock a provider stream runs on (#177): the record's
//! `stream_idle_seconds` and the idle window [`within`] enforces. Paused time, so
//! a window of seconds costs no real waiting.

use super::*;
use serde_json::json;
use std::time::Duration;

fn meta(v: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("stream_idle_seconds".to_string(), v);
    m
}

#[test]
fn the_idle_window_defaults_and_takes_a_positive_number() {
    assert_eq!(
        stream_idle_of(&Map::new()).expect("unset"),
        Duration::from_secs(60)
    );
    // a JSON null is the same as unset
    assert_eq!(
        stream_idle_of(&meta(Value::Null)).expect("null"),
        Duration::from_secs(60)
    );
    assert_eq!(
        stream_idle_of(&meta(json!(2.5))).expect("set"),
        Duration::from_millis(2500)
    );
    assert_eq!(
        stream_idle_of(&meta(json!(90))).expect("int"),
        Duration::from_secs(90)
    );
}

#[test]
fn a_window_that_is_not_a_positive_number_is_refused_by_name() {
    for bad in [
        json!(0),
        json!(-1.5),
        json!("soon"),
        json!(true),
        json!([1]),
    ] {
        let err = stream_idle_of(&meta(bad.clone())).expect_err("refused");
        assert_eq!(err.kind, "ValueError", "{bad}");
        assert!(
            err.message
                .starts_with("stream_idle_seconds must be a positive number of seconds"),
            "{}",
            err.message
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_future_that_answers_within_the_window_passes_its_output() {
    let out = within(Duration::from_secs(5), async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        7
    })
    .await;
    assert_eq!(out, Ok(7));
}

#[tokio::test(start_paused = true)]
async fn a_future_still_pending_when_the_window_passes_is_stalled() {
    let out = within(Duration::from_secs(5), std::future::pending::<()>()).await;
    assert_eq!(out, Err(Stalled { idle_secs: 5.0 }));
}

#[test]
fn a_stall_is_a_raise_naming_the_seconds_and_the_endpoint() {
    let err = Stalled { idle_secs: 0.2 }.error("anthropic_backend", "https://x/v1/messages");
    assert_eq!(err.kind, "RuntimeError");
    assert_eq!(
        err.message,
        "anthropic_backend: stream_stalled — https://x/v1/messages sent no bytes for 0.2s; \
         this turn is NOT complete"
    );
}
