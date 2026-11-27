//! The INBOUND verbs of a wire leg (`web_ws`, `web_rest`) — how an admitted frame
//! becomes the leg's own call. Python `io_bridge._inbound`.
//!
//! IDENTITY COMES ONLY FROM DISPATCH. A transport's loop (the
//! HTTP server, a websocket's receive loop) runs outside every dispatch, so what
//! it sends is the operator's. It never sends a frame onward itself: it hands the
//! admitted frame to its LEG as one of these verbs ([`hand_in`]), and the leg's
//! handler performs the onward call — so that call is the leg's, gated as the leg
//! (by the leg's own lines). Whoever is on the other end of
//! the wire is not an identity in this kernel; the leg is.
//!
//! The leg's `ingress_rule` has already judged the external frame before
//! the transport hands it in. These verbs are gated like any other: the
//! transport's hand-in is the operator's (ungated); an agent calling them needs
//! the grant on the leg like any verb.
//!
//! EVERYTHING ASYNC (#795): the hand-in is a CALL. The transport gets the ACK at
//! once (`{pid, status: accepted}`, [`ack_value`]) and the leg's settle later, as
//! a continuation; a wire leg tells its client that settle as a `settle` payload
//! ([`settle_payload`]). Nothing waits for it.

use std::sync::Arc;

use fantastic_kernel::declared::HandIn;
use fantastic_kernel::{AgentId, Blob, Item, Kernel, Outcome, Status};
use serde_json::{json, Value};

/// THE TRANSPORT'S ONE CALL: hand one admitted frame to the leg `leg` as an
/// inbound verb. Returns the ACK — `accepted` with the leg execution's pid, or a
/// refusal (an `inbound_call` of another shape, a launch refusal), for which
/// `then` never runs. When the leg's execution settles, `then(outcome)` runs with
/// it (its reply body at its slot), as a message to the transport; no task waits
/// for it. Made from the transport's loop — outside every dispatch, so it is the
/// operator's. `body` is a raw request body at its path of `payload`.
pub async fn hand_in<F, Fut>(
    kernel: &Arc<Kernel>,
    leg: &AgentId,
    payload: Value,
    body: Option<Blob>,
    then: F,
) -> Outcome
where
    F: FnOnce(Outcome) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    // An inbound call's `secret` marks (paths into ITS payload) mark the hand-in
    // too: the leg's own call row holds the item, so its values are shown as the
    // onward call's are (`fantastic_kernel::secret`). An `inbound_call` of another
    // shape is refused here, before anything is sent.
    let marks: Vec<String> = match serde_json::from_value::<HandIn>(payload.clone()) {
        Ok(HandIn::InboundCall { item }) => item
            .secret
            .into_iter()
            .map(|m| format!("item.payload.{m}"))
            .collect(),
        Ok(HandIn::Other) => Vec::new(),
        Err(e) => {
            return Outcome::refused(fantastic_kernel::verbs::unparsed("inbound_call", &e.to_string()));
        }
    };
    let mut item = Item::new(leg.clone(), payload).with_secret(marks);
    if let Some(b) = body {
        item = item.with_binary(b.path, b.bytes);
    }
    kernel.call(item, then).await
}

/// An ACK as a wire leg answers it: `{pid, status: "accepted"}` for an accepted
/// call; the refusal itself (its `error`, `reason`, …) for one refused at launch,
/// so a client's existing refusal branch reads it unchanged.
#[must_use]
pub fn ack_value(ack: &Outcome) -> Value {
    if ack.status == Status::Accepted {
        json!({"pid": ack.pid.map(|p| p.get()), "status": Status::Accepted.as_str()})
    } else {
        ack.result.clone()
    }
}

/// Is `data` (a reply frame's `data`, a POST's body) an ACK — a call accepted,
/// whose settle comes later?
#[must_use]
pub fn is_ack(data: &Value) -> bool {
    data.get("status").and_then(Value::as_str) == Some(Status::Accepted.as_str())
        && data.get("error").is_none()
}

/// The SETTLE a wire leg tells its client: `{type: "settle", re, status, data}` —
/// `re` names the call (a frame's id, a POST's pid), `data` is the result.
#[must_use]
pub fn settle_payload(re: Value, out: &Outcome) -> Value {
    json!({"type": "settle", "re": re, "status": out.status.as_str(), "data": out.result})
}
