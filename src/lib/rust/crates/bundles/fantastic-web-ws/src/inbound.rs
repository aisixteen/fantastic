//! The INBOUND verbs of a wire leg (`web_ws`, `web_rest`) — how an admitted frame
//! becomes the leg's own call. Python `io_bridge._inbound`.
//!
//! IDENTITY COMES ONLY FROM DISPATCH. A transport's loop (the HTTP server, a
//! websocket's receive loop) runs outside every dispatch, so what it sends is the
//! operator's. It never sends a frame onward itself: it hands the admitted frame to
//! its LEG as one of these verbs (`fantastic_io_bridge::inbound::hand_in`), and the
//! leg's handler performs the onward call here — so that call is the leg's, gated as
//! the leg (its `can_call`, the target's `callable_by`).
//!
//! The leg's `ingress_rule` has already judged the frame before the transport hands
//! it in. These verbs are gated like any other: the transport's hand-in is the
//! operator's (ungated); an agent calling them needs the grant on the leg like any verb.

use crate::verbs::{InboundCall, InboundEmit, InboundWatch};
use fantastic_kernel::{AgentId, Blob, Item, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// Send the item onward as the leg; the reply comes back with the raw body at its
/// slot of the reply (an onward call answering a chunk).
pub async fn call(
    a: InboundCall,
    body: Option<Blob>,
    kernel: &Arc<Kernel>,
) -> (Value, Option<Blob>) {
    let item = a.item;
    let mut inner = item.payload;
    // The body rode at `item.payload.<slot>`; onward it sits at `<slot>` of the
    // item's own payload, the slot named again where the frame named it.
    let onward_body = body
        .and_then(|b| b.under("item"))
        .and_then(|b| b.under("payload"));
    if let Some(b) = &onward_body {
        let _ = b.place(&mut inner, Value::Null);
    }
    let mut onward = Item::new(AgentId::from(item.target.as_str()), inner);
    if let Some(id) = item.id {
        onward = onward.with_id(id);
    }
    if let Some(marks) = item.secret {
        onward = onward.with_secret(marks);
    }
    if let Some(b) = onward_body {
        onward = onward.with_binary(b.path, b.bytes);
    }
    let out = kernel.send(vec![onward]).await.remove(0);
    // The onward reply's own secret paths (a credential store's `get` fields) are
    // rooted at the reply, which IS this leg's reply: the leg's settle row hides
    // them too, and the transport can tell the far side (`Outcome::secret`).
    fantastic_kernel::note_reply_secret(out.secret.clone());
    (out.result, out.body)
}

/// Emit the payload on the target as the leg.
pub async fn emit(a: InboundEmit, body: Option<Blob>, kernel: &Arc<Kernel>) -> Value {
    let mut inner = Value::Object(a.payload);
    let item = match body.and_then(|b| b.under("payload")) {
        Some(b) => {
            let _ = b.place(&mut inner, Value::Null);
            fantastic_kernel::inbox::InboxItem::binary(inner, b)
        }
        None => fantastic_kernel::inbox::InboxItem::text(inner),
    };
    kernel.emit_item(&AgentId::from(a.target.as_str()), item).await;
    Value::Null
}

/// Mirror `src`'s traffic into `reader` as the leg.
pub async fn watch(a: InboundWatch, kernel: &Arc<Kernel>) -> Value {
    match kernel
        .watch(&AgentId::from(a.src.as_str()), AgentId::from(a.reader.as_str()))
        .await
    {
        Some(denial) => denial,
        None => json!({"watching": a.src}),
    }
}

/// May the leg tap the kernel-wide state stream?
pub fn state_subscribe(leg: &AgentId, kernel: &Arc<Kernel>) -> Value {
    match kernel.authorize_state_subscribe(leg) {
        Some(denial) => denial,
        None => json!({"subscribed": true}),
    }
}
