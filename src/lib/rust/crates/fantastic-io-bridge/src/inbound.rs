//! The INBOUND verbs of a wire leg (`web_ws`, `web_rest`) — how an admitted frame
//! becomes the leg's own call. Python `io_bridge._inbound`.
//!
//! IDENTITY COMES ONLY FROM DISPATCH. A transport's loop (the
//! HTTP server, a websocket's receive loop) runs outside every dispatch, so what
//! it sends is the operator's. It never sends a frame onward itself: it hands the
//! admitted frame to its LEG as one of these verbs ([`hand_in`]), and the leg's
//! handler performs the onward call — so that call is the leg's, gated as the leg
//! (its `can_call`, the target's `callable_by`). Whoever is on the other end of
//! the wire is not an identity in this kernel; the leg is.
//!
//! The leg's `ingress_rule` has already judged the frame (`gate_inbound`) before
//! the transport hands it in. These verbs are gated like any other: the
//! transport's hand-in is the operator's (ungated); an agent calling them needs
//! the grant on the leg like any verb.

use std::sync::Arc;

use fantastic_kernel::declared::HandIn;
use fantastic_kernel::{AgentId, Blob, Item, Kernel, Outcome};
use serde_json::Value;

/// THE TRANSPORT'S ONE SEND: hand one admitted frame to the leg `leg` as an
/// inbound verb and return the leg's outcome (its reply body at its slot). Made
/// from the transport's loop — outside every dispatch, so it is the operator's.
/// `body` is a raw request body at its path of `payload`.
pub async fn hand_in(
    kernel: &Arc<Kernel>,
    leg: &AgentId,
    payload: Value,
    body: Option<Blob>,
) -> Outcome {
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
    kernel.send(vec![item]).await.remove(0)
}
