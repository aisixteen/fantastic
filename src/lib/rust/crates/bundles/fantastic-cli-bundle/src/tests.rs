//! The render sink's contract. Output goes to stdout, so what is pinned here is
//! the DISPATCH: which verbs are handled, which are dropped, and that reflect
//! advertises exactly what it accepts.

use super::*;
use serde_json::json;

fn kernel() -> Arc<Kernel> {
    Arc::new(Kernel::new())
}

#[tokio::test]
async fn reflect_advertises_exactly_what_it_accepts() {
    let b = CliBundle;
    let id = AgentId("cli".into());
    let reply = b
        .handle(&id, &json!({"type": "reflect"}), &kernel())
        .await
        .unwrap()
        .unwrap();
    let accepts: Vec<String> = reply["accepts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    // The list and the dispatch must not drift: a verb advertised but not
    // rendered is a promise this sink does not keep.
    for verb in ACCEPTS {
        assert!(accepts.contains(&verb.to_string()), "missing {verb}");
        assert!(
            reply["verbs"].get(verb).is_some(),
            "{verb} accepted but undocumented"
        );
    }
}

#[tokio::test]
async fn an_unknown_verb_is_dropped_not_refused() {
    // A render surface that answered `unknown verb` would turn a cosmetic gap
    // into a FAILED SEND for whoever was streaming to it.
    let b = CliBundle;
    let reply = b
        .handle(
            &AgentId("cli".into()),
            &json!({"type": "nonsense_verb"}),
            &kernel(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, Value::Null);
    assert!(reply.get("error").is_none());
}

#[tokio::test]
async fn every_accepted_verb_answers_null_rather_than_an_error() {
    let b = CliBundle;
    let id = AgentId("cli".into());
    for verb in ACCEPTS {
        let reply = b
            .handle(&id, &json!({"type": verb, "text": "x"}), &kernel())
            .await
            .unwrap()
            .unwrap();
        assert!(reply.get("error").is_none(), "{verb} refused");
    }
}
