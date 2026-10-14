//! The INBOUND verbs of a wire leg (`web_ws`, `web_rest`) — how an admitted frame
//! becomes the leg's own call. Python `io_bridge._inbound`.
//!
//! IDENTITY COMES ONLY FROM DISPATCH (owner 2026-10-04). A transport's loop (the
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

use fantastic_kernel::refusal::bad;
use fantastic_kernel::{AgentId, Blob, Item, Kernel, Outcome};
use serde_json::{json, Value};

/// The inbound verbs, in python's order.
pub const VERBS: [&str; 4] = [
    "inbound_call",
    "inbound_emit",
    "inbound_watch",
    "inbound_state_subscribe",
];

/// Each inbound verb's doc line — python's docstrings, for a leg's reflect.
#[must_use]
pub fn verb_docs() -> [(&'static str, &'static str); 4] {
    [
        (
            "inbound_call",
            "args: item:{target, payload, id?} (req). Sends `item` onward AS THIS LEG and returns the onward call's reply — how an admitted inbound call frame becomes the leg's own call, gated as the leg.",
        ),
        (
            "inbound_emit",
            "args: target:str (req), payload:dict (req). Emits `payload` on `target` AS THIS LEG (gated as the leg, verb `emit`) — how an admitted inbound emit frame becomes the leg's own emit.",
        ),
        (
            "inbound_watch",
            "args: src:str (req — an agent id), reader:str (req — a registered inbox). Mirrors `src`'s traffic into `reader` AS THIS LEG (gated as the leg, verb `watch`). Returns {watching:src} or the refusal.",
        ),
        (
            "inbound_state_subscribe",
            "No args. May THIS LEG tap the kernel-wide state stream (gated on the root, verb `state_subscribe`)? Returns {subscribed:true} or the refusal; the transport then wires the tap.",
        ),
    ]
}

/// Serve one inbound verb on `leg` — `None` when `payload.type` is not one. The
/// reply comes back with the raw body at its slot of the reply (an onward call
/// answering a chunk). Runs inside the leg's own dispatch, so every send here is
/// the leg's.
pub async fn handle(
    leg: &AgentId,
    payload: &Value,
    body: Option<Blob>,
    kernel: &Arc<Kernel>,
) -> Option<(Value, Option<Blob>)> {
    let verb = payload.get("type").and_then(Value::as_str)?;
    Some(match verb {
        "inbound_call" => inbound_call(payload, body, kernel).await,
        "inbound_emit" => (inbound_emit(payload, body, kernel).await, None),
        "inbound_watch" => (inbound_watch(payload, kernel).await, None),
        "inbound_state_subscribe" => (inbound_state_subscribe(leg, kernel), None),
        _ => return None,
    })
}

async fn inbound_call(
    payload: &Value,
    body: Option<Blob>,
    kernel: &Arc<Kernel>,
) -> (Value, Option<Blob>) {
    let Some(item) = payload.get("item").and_then(Value::as_object) else {
        return (
            bad(
                "inbound_call",
                "item",
                "a batch item {target, payload, id?}",
            ),
            None,
        );
    };
    let target = item
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut inner = item.get("payload").cloned().unwrap_or(Value::Null);
    // The body rode at `item.payload.<slot>`; onward it sits at `<slot>` of the
    // item's own payload, the slot named again where the frame named it.
    let onward_body = body
        .and_then(|b| b.under("item"))
        .and_then(|b| b.under("payload"));
    if let Some(b) = &onward_body {
        let _ = b.place(&mut inner, Value::Null);
    }
    let mut onward = Item::new(AgentId::from(target), inner);
    if let Some(id) = item.get("id").filter(|v| !v.is_null()) {
        onward = onward.with_raw_id(id.clone());
    }
    if let Some(marks) = item.get("secret").filter(|v| !v.is_null()) {
        onward = onward.with_raw_secret(marks.clone());
    }
    if let Some(b) = onward_body {
        onward = onward.with_binary(b.path, b.bytes);
    }
    let out = kernel.send(vec![onward]).await.remove(0);
    (out.result, out.body)
}

async fn inbound_emit(payload: &Value, body: Option<Blob>, kernel: &Arc<Kernel>) -> Value {
    let Some(target) = payload
        .get("target")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return bad("inbound_emit", "target", "an agent id");
    };
    let Some(inner) = payload.get("payload").filter(|v| v.is_object()) else {
        return bad("inbound_emit", "payload", "an object");
    };
    let mut inner = inner.clone();
    let item = match body.and_then(|b| b.under("payload")) {
        Some(b) => {
            let _ = b.place(&mut inner, Value::Null);
            fantastic_kernel::inbox::InboxItem::binary(inner, b)
        }
        None => fantastic_kernel::inbox::InboxItem::text(inner),
    };
    kernel.emit_item(&AgentId::from(target), item).await;
    Value::Null
}

async fn inbound_watch(payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let Some(src) = payload
        .get("src")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return bad("inbound_watch", "src", "an agent id");
    };
    let Some(reader) = payload
        .get("reader")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return bad("inbound_watch", "reader", "a registered inbox id");
    };
    match kernel
        .watch(&AgentId::from(src), AgentId::from(reader))
        .await
    {
        Some(denial) => denial,
        None => json!({"watching": src}),
    }
}

fn inbound_state_subscribe(leg: &AgentId, kernel: &Arc<Kernel>) -> Value {
    match kernel.authorize_state_subscribe(leg) {
        Some(denial) => denial,
        None => json!({"subscribed": true}),
    }
}

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
    // onward call's are (`fantastic_kernel::secret`).
    let marks: Option<Vec<String>> = payload
        .get("item")
        .and_then(|i| i.get("secret"))
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .map(|m| m.as_str().map(|m| format!("item.payload.{m}")))
                .collect()
        });
    let mut item = Item::new(leg.clone(), payload);
    if let Some(m) = marks {
        item = item.with_secret(m);
    }
    if let Some(b) = body {
        item = item.with_binary(b.path, b.bytes);
    }
    kernel.send(vec![item]).await.remove(0)
}
