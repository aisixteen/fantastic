//! Images in a chat (#622): attached to a `send`, kept as bytes in the chat's store, shown
//! to a model that can see them in the request where it is to, an anchor from the next one,
//! alt text for a model that cannot, and the capability map the chat keeps of its provider.

use super::*;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use std::io::Cursor;

const GO: &str = r#"ok <call name="done"></call>"#;

/// A png of `w`x`h` with some detail in it.
fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([
            (x * 7 % 256) as u8,
            (y * 13 % 256) as u8,
            ((x * y) % 256) as u8,
        ])
    });
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

async fn ask(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(agent.clone(), payload)])
        .await
        .remove(0)
        .result
}

async fn update(kernel: &Arc<Kernel>, agent: &AgentId, fields: Value) {
    let mut p = json!({"type": "update_agent", "id": agent.as_str()});
    for (k, v) in fields.as_object().unwrap() {
        p[k] = v.clone();
    }
    let r = ask(kernel, &AgentId::from("kernel_state"), p).await;
    assert!(r.get("error").is_none(), "{r}");
}

async fn rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    ask(kernel, agent, json!({"type": "rows"})).await["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// What a chat's provider says it can do: the entry of the model `test-model`.
fn caps(images: bool, max_images: i64, max_bytes: i64, max_pixels: i64) -> Value {
    let entry = if images {
        json!({"images": true, "max_images": max_images, "max_bytes": max_bytes, "max_pixels": max_pixels})
    } else {
        json!({})
    };
    json!({"provider": "x", "model": "test-model", "capabilities": entry})
}

/// A chat on the mock backend that reads files through its bridge (`file_bridge_id`), with
/// `pic.png` (`w`x`h`) in the bridge's folder, and what its provider can do as `caps`.
async fn chat_with(
    tmp: &TempDir,
    tag: &str,
    caps: Value,
) -> (Arc<Kernel>, AgentId, Arc<MockScript>) {
    let (kernel, chat) = mk_kernel(tmp, tag).await;
    update(
        &kernel,
        &chat,
        json!({"file_bridge_id": format!("ff_{chat}"), "provider_capabilities": caps}),
    )
    .await;
    let script = script(&chat, (0..12).map(|_| pass(GO)).collect(), Duration::ZERO);
    (kernel, chat, script)
}

fn put_file(tmp: &TempDir, name: &str, bytes: &[u8]) {
    let dir = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), bytes).unwrap();
}

async fn say(kernel: &Arc<Kernel>, chat: &AgentId, text: &str, images: Value) -> Value {
    ask(
        kernel,
        chat,
        json!({"type": "send", "text": text, "images": images}),
    )
    .await
}

/// The bytes the chat's store holds under `image_ref`.
async fn stored(kernel: &Arc<Kernel>, chat: &AgentId, image_ref: &str) -> Option<Vec<u8>> {
    fantastic_kernel::send::as_agent(chat, async {
        crate::memory::get_bytes(chat, kernel, image_ref)
            .await
            .ok()
            .map(|(_, b)| b)
    })
    .await
}

fn user_rows(all: &[Value]) -> Vec<Value> {
    all.iter()
        .filter(|r| r["role"] == "user" && r.get("kind").is_none())
        .cloned()
        .collect()
}

/// The message of a request that carries `text`.
fn message<'a>(request: &'a [Value], text: &str) -> &'a Value {
    request
        .iter()
        .find(|m| m["content"].as_str().is_some_and(|c| c.starts_with(text)))
        .unwrap_or_else(|| panic!("no message starting {text:?} in {request:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attached_image_is_stored_as_bytes_and_sits_in_the_row_as_a_part() {
    let tmp = TempDir::new().unwrap();
    let (kernel, chat, script) =
        chat_with(&tmp, "img_row", caps(true, 5, 5_000_000, 1_150_000)).await;
    let raw = png(64, 48);
    put_file(&tmp, "pic.png", &raw);
    let r = say(&kernel, &chat, "look", json!([{"path": "pic.png"}])).await;
    assert!(r.get("error").is_none(), "{r}");
    let row = user_rows(&rows(&kernel, &chat).await).remove(0);
    assert_eq!(
        row["content"],
        json!("look"),
        "the image is not in `content`"
    );
    let part = &row["images"][0];
    assert_eq!(part["type"], json!("image"));
    assert_eq!(part["needs"], json!("images"));
    assert_eq!(part["media_type"], json!("image/png"));
    assert_eq!(
        (part["width"].clone(), part["height"].clone()),
        (json!(64), json!(48))
    );
    assert_eq!(part["bytes"], json!(raw.len()));
    assert!(
        part["hash"].as_str().is_some_and(|h| h.len() == 16),
        "{part}"
    );
    let info = &row["attributes"]["image"][0];
    assert_eq!(info["name"], json!("pic.png"));
    assert_eq!(
        info["original"],
        json!({"width": 64, "height": 48, "bytes": raw.len()})
    );
    assert_eq!(
        info["stored"], info["original"],
        "inside the limits: kept as it came"
    );
    assert_eq!(info["tokens"], json!(64 * 48 / 750));
    // the bytes are in the store, raw, and nowhere in the listing
    assert_eq!(
        stored(&kernel, &chat, part["ref"].as_str().unwrap()).await,
        Some(raw)
    );
    let listing = ask(&kernel, &chat, json!({"type": "history"}))
        .await
        .to_string();
    assert!(!listing.contains("base64"), "{listing}");
    assert_eq!(script.passes_served(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_that_sees_gets_the_image_once_then_an_anchor_and_recall_brings_it_back() {
    let tmp = TempDir::new().unwrap();
    let (kernel, chat, script) =
        chat_with(&tmp, "img_see", caps(true, 5, 5_000_000, 1_150_000)).await;
    put_file(&tmp, "pic.png", &png(64, 48));
    say(&kernel, &chat, "look", json!([{"path": "pic.png"}])).await;
    // the request where the model is to see it: the part rides on the message
    let first = script.request_messages(0);
    let sent = message(&first, "look");
    assert_eq!(sent["images"].as_array().map(Vec::len), Some(1), "{sent}");
    assert_eq!(sent["content"], json!("look"));
    let id = user_rows(&rows(&kernel, &chat).await)[0]["id"]
        .as_i64()
        .unwrap();
    // the next request: the model has answered, the image is an anchor
    say(&kernel, &chat, "and now", json!(null)).await;
    let second = script.request_messages(1);
    let old = message(&second, "look");
    assert!(old.get("images").is_none(), "{old}");
    let anchor =
        format!("[image \u{b7} 64\u{d7}48 \u{b7} image/png \u{b7} ~4 tokens \u{b7} id {id}]");
    assert_eq!(old["content"], json!(format!("look\n{anchor}")));
    // recall brings it back, once
    let back = ask(&kernel, &chat, json!({"type": "recall", "id": id})).await;
    assert!(
        back["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("come back"),
        "{back}"
    );
    say(&kernel, &chat, "again", json!(null)).await;
    let third = script.request_messages(2);
    assert_eq!(
        message(&third, "look")["images"].as_array().map(Vec::len),
        Some(1)
    );
    say(&kernel, &chat, "once more", json!(null)).await;
    let fourth = script.request_messages(3);
    assert!(message(&fourth, "look").get("images").is_none());
    // the requests' parts name the forms
    let all: String = rows(&kernel, &chat)
        .await
        .iter()
        .filter(|r| r["role"] == "assistant")
        .map(|r| r["attributes"]["parts"].to_string())
        .collect();
    for form in ["image", "image_anchor", "image_recalled"] {
        assert!(
            all.contains(&format!("\"form\":\"{form}\"")),
            "no {form}: {all}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_that_cannot_see_gets_alt_text_and_never_the_bytes() {
    let tmp = TempDir::new().unwrap();
    let (kernel, chat, script) = chat_with(&tmp, "img_alt", caps(false, 0, 0, 0)).await;
    put_file(&tmp, "pic.png", &png(64, 48));
    say(&kernel, &chat, "look", json!([{"path": "pic.png"}])).await;
    let sent = script.request_messages(0);
    let m = message(&sent, "look");
    assert!(m.get("images").is_none(), "{m}");
    assert_eq!(
        m["content"],
        json!("look\n[image: pic.png \u{b7} 64\u{d7}48 \u{b7} not shown to this model]")
    );
    // the row still holds the part, and the store the bytes
    let row = user_rows(&rows(&kernel, &chat).await).remove(0);
    assert!(
        stored(&kernel, &chat, row["images"][0]["ref"].as_str().unwrap())
            .await
            .is_some()
    );
    say(&kernel, &chat, "next", json!(null)).await;
    let again = script.request_messages(1);
    assert!(message(&again, "look")["content"]
        .as_str()
        .unwrap()
        .contains("not shown to this model"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_chat_goes_from_an_image_model_to_a_text_model_and_back() {
    let tmp = TempDir::new().unwrap();
    let image_model = caps(true, 5, 5_000_000, 1_150_000);
    let (kernel, chat, script) = chat_with(&tmp, "img_swap", image_model.clone()).await;
    put_file(&tmp, "pic.png", &png(64, 48));
    say(&kernel, &chat, "first", json!([{"path": "pic.png"}])).await;
    let before = rows(&kernel, &chat).await;
    let part = user_rows(&before)[0]["images"][0].clone();
    // a model that cannot see
    update(
        &kernel,
        &chat,
        json!({"provider_capabilities": caps(false, 0, 0, 0)}),
    )
    .await;
    say(&kernel, &chat, "second", json!([{"ref": part["ref"]}])).await;
    let text_only = script.request_messages(1);
    assert!(message(&text_only, "first")["content"]
        .as_str()
        .unwrap()
        .contains("not shown to this model"));
    assert!(message(&text_only, "second")["content"]
        .as_str()
        .unwrap()
        .contains("not shown to this model"));
    assert!(
        text_only.iter().all(|m| m.get("images").is_none()),
        "{text_only:?}"
    );
    // and back to one that can: the same bytes, shown again
    update(
        &kernel,
        &chat,
        json!({"provider_capabilities": image_model}),
    )
    .await;
    say(&kernel, &chat, "third", json!([{"ref": part["ref"]}])).await;
    let back = script.request_messages(2);
    assert_eq!(
        message(&back, "third")["images"].as_array().map(Vec::len),
        Some(1),
        "{back:?}"
    );
    assert!(
        message(&back, "first").get("images").is_none(),
        "an answered row is an anchor"
    );
    // the store was never changed by any of it: the first part is still the first row's
    let after = rows(&kernel, &chat).await;
    assert_eq!(user_rows(&after)[0]["images"][0], part);
    assert!(stored(&kernel, &chat, part["ref"].as_str().unwrap())
        .await
        .is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_image_over_the_limits_after_the_downscale_or_not_an_image_is_refused() {
    let tmp = TempDir::new().unwrap();
    // 100 bytes cannot hold any image of this size, however small it is made
    let (kernel, chat, script) = chat_with(&tmp, "img_over", caps(true, 1, 100, 1_000)).await;
    put_file(&tmp, "pic.png", &png(64, 48));
    put_file(&tmp, "text.png", b"this is not an image at all");
    for (images, why) in [
        (json!([{"path": "pic.png"}]), "after scaling it down"),
        (json!([{"path": "text.png"}]), "is not an image"),
        (
            json!([{"path": "pic.png"}, {"path": "pic.png"}]),
            "takes 1 per request",
        ),
        (json!([{"path": "pic.png", "ref": "x"}]), "exactly one of"),
        (json!([{"path": "../outside.png"}]), "read_stream"),
    ] {
        let r = say(&kernel, &chat, "look", images).await;
        assert!(
            r["error"].as_str().is_some_and(|e| e.contains(why)),
            "{why}: {r}"
        );
        assert_eq!(r["reason"], json!("bad_arg"));
    }
    assert_eq!(script.passes_served(), 0, "a refused send runs no turn");
    assert!(rows(&kernel, &chat).await.is_empty(), "and stores no row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_image_larger_than_the_model_takes_is_scaled_down_and_the_original_size_is_kept() {
    let tmp = TempDir::new().unwrap();
    let (kernel, chat, _script) =
        chat_with(&tmp, "img_scale", caps(true, 5, 5_000_000, 1_000)).await;
    put_file(&tmp, "big.png", &png(200, 100));
    say(&kernel, &chat, "look", json!([{"path": "big.png"}])).await;
    let row = user_rows(&rows(&kernel, &chat).await).remove(0);
    let part = &row["images"][0];
    let (w, h) = (
        part["width"].as_u64().unwrap(),
        part["height"].as_u64().unwrap(),
    );
    assert!(w * h <= 1_000 && w * h > 500, "{w}x{h}");
    assert_eq!(w / h, 2, "the proportions stay");
    let info = &row["attributes"]["image"][0];
    assert_eq!(info["original"]["width"], json!(200));
    assert_eq!(info["stored"]["width"], json!(w));
    let bytes = stored(&kernel, &chat, part["ref"].as_str().unwrap())
        .await
        .unwrap();
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!(
        (u64::from(decoded.width()), u64::from(decoded.height())),
        (w, h)
    );
    assert_eq!(part["bytes"], json!(bytes.len()));
    // a chat setting lowers the limit further
    update(&kernel, &chat, json!({"image_max_pixels": 200})).await;
    say(&kernel, &chat, "again", json!([{"path": "big.png"}])).await;
    let newest = user_rows(&rows(&kernel, &chat).await).pop().unwrap();
    let p = &newest["images"][0];
    assert!(
        p["width"].as_u64().unwrap() * p["height"].as_u64().unwrap() <= 200,
        "{p}"
    );
}

// ── the capability map ────────────────────────────────────────────────────────

/// A provider that answers `reflect` with the `reflect` of its record (counting the asks)
/// and keeps the images that reach it on the binary path.
struct Pics;

type Seen = std::sync::Mutex<Vec<(String, String, Vec<u8>)>>;
static SEEN: std::sync::OnceLock<Seen> = std::sync::OnceLock::new();

fn seen() -> &'static Seen {
    SEEN.get_or_init(Seen::default)
}

#[async_trait]
impl Bundle for Pics {
    fn name(&self) -> &str {
        "pics"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match payload["type"].as_str() {
            Some("reflect") => {
                let meta = helpers::agent_meta(id, kernel);
                match meta.get("reflect_refuses") {
                    Some(_) => Ok(Some(json!({"error": "refused", "reason": "unauthorized"}))),
                    None => Ok(Some(
                        meta.get("reflect_reply").cloned().unwrap_or(json!({})),
                    )),
                }
            }
            Some("complete") => Ok(Some(
                json!({"rid": payload["rid"], "model": "m", "prompt_tokens": 1,
                                               "completion_tokens": 1, "stop_reason": "end_turn"}),
            )),
            _ => Ok(Some(json!({"error": "no such verb"}))),
        }
    }
    async fn handle_binary(
        &self,
        id: &AgentId,
        header: Value,
        body: Option<fantastic_kernel::outcome::Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<fantastic_kernel::outcome::Blob>), BundleError> {
        if let (Some(image), Some(bytes)) =
            (header.get("image"), body.and_then(|b| b.into_at("data")))
        {
            seen().lock().unwrap().push((
                header["rid"].as_str().unwrap().to_string(),
                image["ref"].as_str().unwrap().to_string(),
                bytes,
            ));
            return Ok((Some(json!({"rid": header["rid"]})), None));
        }
        Ok((self.handle(id, &header, kernel).await?, None))
    }
}

/// A chat on the mock backend whose `provider_id` names the stub provider `capp`.
async fn chat_with_provider(
    tmp: &TempDir,
    tag: &str,
    reply: Value,
) -> (Arc<Kernel>, AgentId, String) {
    let kernel = base_kernel(tmp, |k| k.bundles.register("pics.tools", Pics)).await;
    let id = id_for(tmp, tag);
    let file_id = format!("ff_{id}");
    add_file_bridge(&kernel, &file_id, &tmp.path().join(".fantastic"), &id).await;
    let chat = add_backend(&kernel, &id, &file_id).await;
    let root = kernel.root().unwrap();
    let provider = format!("capp_{tag}");
    kernel.register(Agent::new(
        AgentId::from(provider.as_str()),
        Some("pics.tools".into()),
        Some(root.id.clone()),
        json!({"callable_by": {id.as_str(): "*"}, "reflect_reply": reply})
            .as_object()
            .unwrap()
            .clone(),
        tmp.path().join("capp"),
        false,
    ));
    let mut can_call = helpers::agent_meta(&chat, &kernel)["can_call"]
        .as_object()
        .unwrap()
        .clone();
    can_call.insert(provider.clone(), json!("*"));
    update(
        &kernel,
        &chat,
        json!({"provider_id": provider, "can_call": can_call, "file_bridge_id": file_id}),
    )
    .await;
    script(&chat, (0..12).map(|_| pass(GO)).collect(), Duration::ZERO);
    (kernel, chat, provider)
}

fn held(kernel: &Arc<Kernel>, chat: &AgentId) -> Value {
    helpers::agent_meta(chat, kernel)
        .get("provider_capabilities")
        .cloned()
        .unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chat_keeps_its_providers_capabilities_and_asks_again_only_on_a_change() {
    let tmp = TempDir::new().unwrap();
    let e1 =
        json!({"images": true, "max_images": 3, "max_bytes": 5_000_000, "max_pixels": 1_150_000});
    let e2 =
        json!({"images": true, "max_images": 9, "max_bytes": 4_000_000, "max_pixels": 900_000});
    let reply = json!({"capabilities": {"test-model": e1, "small-model": {"images": false}}});
    let (kernel, chat, provider) = chat_with_provider(&tmp, "caps_map", reply).await;
    let provider_id = AgentId::from(provider.as_str());
    assert_eq!(
        held(&kernel, &chat),
        Value::Null,
        "nothing before the first turn"
    );
    // every write of the map to the chat's record shows as an `updated` event naming it
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (counter, chat_id) = (writes.clone(), chat.as_str().to_string());
    kernel.add_state_subscriber(Arc::new(move |ev, _| {
        let names = ev["changed"]
            .as_array()
            .is_some_and(|c| c.iter().any(|k| k == "provider_capabilities"));
        if ev["kind"] == "updated" && ev["agent_id"] == chat_id && names {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }));
    let written = || writes.load(std::sync::atomic::Ordering::SeqCst);
    say(&kernel, &chat, "hello", json!(null)).await;
    assert_eq!(written(), 1, "the first read writes the map once");
    assert_eq!(
        held(&kernel, &chat),
        json!({"provider": provider, "model": "test-model", "capabilities": e1}),
        "read from the provider when the chat starts"
    );
    // the provider now says something else, but nothing the chat is set to has changed: not asked
    let other = json!({"capabilities": {"test-model": e2, "small-model": e2}});
    update(&kernel, &provider_id, json!({"reflect_reply": other})).await;
    say(&kernel, &chat, "again", json!(null)).await;
    assert_eq!(held(&kernel, &chat)["capabilities"], e1);
    assert_eq!(written(), 1, "an unchanged turn writes no record");
    // the model changes: asked again, and the entry is the new model's
    update(&kernel, &chat, json!({"model": "small-model"})).await;
    say(&kernel, &chat, "third", json!(null)).await;
    assert_eq!(held(&kernel, &chat)["capabilities"], e2);
    assert_eq!(held(&kernel, &chat)["model"], json!("small-model"));
    // a model the provider does not know has no capability
    update(&kernel, &chat, json!({"model": "unheard-of"})).await;
    say(&kernel, &chat, "fourth", json!(null)).await;
    assert_eq!(held(&kernel, &chat)["capabilities"], json!({}));
    assert!(!crate::images::can(
        &helpers::agent_meta(&chat, &kernel),
        "images"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_incarnation_asks_again_and_keeps_the_map_when_the_provider_does_not_answer() {
    let tmp = TempDir::new().unwrap();
    let e1 =
        json!({"images": true, "max_images": 3, "max_bytes": 5_000_000, "max_pixels": 1_150_000});
    let e2 =
        json!({"images": true, "max_images": 4, "max_bytes": 4_000_000, "max_pixels": 900_000});
    let (kernel, chat, provider) = chat_with_provider(
        &tmp,
        "caps_boot",
        json!({"capabilities": {"test-model": e1}}),
    )
    .await;
    let provider_id = AgentId::from(provider.as_str());
    say(&kernel, &chat, "hello", json!(null)).await;
    assert_eq!(held(&kernel, &chat)["capabilities"], e1);
    // a restart asks the provider once more
    update(
        &kernel,
        &provider_id,
        json!({"reflect_reply": {"capabilities": {"test-model": e2}}}),
    )
    .await;
    ask(&kernel, &chat, json!({"type": "boot"})).await;
    say(&kernel, &chat, "again", json!(null)).await;
    let second = held(&kernel, &chat);
    assert_eq!(second["capabilities"], e2);
    // a provider that refuses leaves the hydrated map as it was, and images work from it
    update(&kernel, &provider_id, json!({"reflect_refuses": true})).await;
    ask(&kernel, &chat, json!({"type": "boot"})).await;
    put_file(&tmp, "pic.png", &png(32, 32));
    say(&kernel, &chat, "look", json!([{"path": "pic.png"}])).await;
    assert_eq!(held(&kernel, &chat), second);
    let script = script_for(&chat);
    let sent = script.request_messages(script.passes_served() - 1);
    assert_eq!(
        message(&sent, "look")["images"].as_array().map(Vec::len),
        Some(1),
        "{sent:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_images_of_a_request_reach_the_provider_on_the_binary_path_before_the_pass() {
    let tmp = TempDir::new().unwrap();
    let k = base_kernel(&tmp, |k| k.bundles.register("pics.tools", Pics)).await;
    let root = k.root().unwrap();
    let meta =
        fantastic_ram_store::test_support::memory_meta(&k, "chat", json!({"pics": "*"})).await;
    k.register(Agent::new(
        AgentId::from("chat"),
        None,
        Some(root.id.clone()),
        meta,
        tmp.path().join("chat"),
        false,
    ));
    k.register(Agent::new(
        AgentId::from("pics"),
        Some("pics.tools".into()),
        Some(root.id.clone()),
        json!({"callable_by": {"chat": "*"}})
            .as_object()
            .unwrap()
            .clone(),
        tmp.path().join("pics"),
        false,
    ));
    let chat = AgentId::from("chat");
    let raw = png(16, 16);
    let part = json!({"type": "image", "needs": "images", "ref": "img-7", "media_type": "image/png",
                      "width": 16, "height": 16, "bytes": raw.len(), "hash": "x"});
    let rp = crate::remote_provider::RemoteProvider::new(&k, &chat, "pics");
    let sent = fantastic_kernel::send::as_agent(&chat, async {
        crate::memory::put_bytes(&chat, &k, "img-7", "image/png", raw.clone())
            .await
            .unwrap();
        let messages = [json!({"role": "user", "content": "see", "images": [part]})];
        let s = futures_util::StreamExt::collect::<Vec<_>>(
            crate::provider::Provider::chat(&rp, &messages, None)
                .await
                .unwrap(),
        )
        .await;
        s.len()
    })
    .await;
    assert_eq!(sent, 0);
    let got = seen().lock().unwrap().clone();
    let mine: Vec<_> = got
        .iter()
        .filter(|(rid, _, _)| rid.starts_with("chat~"))
        .collect();
    assert_eq!(mine.len(), 1, "{got:?}");
    assert_eq!((mine[0].1.as_str(), &mine[0].2), ("img-7", &raw));
}
