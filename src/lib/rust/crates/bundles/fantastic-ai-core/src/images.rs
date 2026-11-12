//! IMAGES IN A CHAT (#622).
//!
//! An image is attached to a `send` (`images: [{path | ref}]`): read (a `path`, through the
//! chat's `file_bridge_id`) or fetched (a `ref` already in the chat's store), decoded with the
//! `image` crate, scaled down to the limits of the current model, and kept as raw bytes in the
//! chat's own memory store (`put_bytes`). The user's row holds the PART, never the bytes:
//! `images: [{type: "image", needs: "images", ref, media_type, width, height, bytes, hash}]`,
//! and its `attributes.image` the sizes and the token estimate.
//!
//! The chat keeps what its provider can do in its own record, `provider_capabilities`
//! (`{provider, model, capabilities: {<key>: …}}`), refreshed from the provider's `reflect`
//! when it starts and whenever `provider_id` or `model` change ([`refresh`]). A part is MARKED
//! with the capability it needs (`needs`); the context build matches it against that map and
//! the row renders as one of the forms below. The store is never changed by it.

use std::io::Cursor;
use std::sync::Arc;

use fantastic_kernel::{AgentId, Item, Kernel};
use image::{DynamicImage, ImageFormat};
use serde_json::{json, Map, Value};

use crate::state::BackendState;

/// The capability an image part needs.
pub(crate) const NEEDS: &str = "images";

/// The chat record's field holding its provider's capabilities.
const FIELD: &str = "provider_capabilities";

/// A chat setting that lowers the pixel limit of the model (a number).
const SETTING_PIXELS: &str = "image_max_pixels";

/// A chat setting that lowers the byte limit of the model (a number).
const SETTING_BYTES: &str = "image_max_bytes";

/// The re-encodes a too large image gets before it is refused.
const SHRINKS: u32 = 10;

/// What the model can take of images: the capability entry of the chat's model, lowered by
/// the chat's own settings.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// How many images one request may carry (`None`: no bound named).
    pub max_images: Option<i64>,
    /// The bytes of one image.
    pub max_bytes: i64,
    /// The pixels of one image.
    pub max_pixels: i64,
}

fn entry_of(meta: &Map<String, Value>) -> Option<&Map<String, Value>> {
    meta.get(FIELD)?.get("capabilities")?.as_object()
}

/// Whether the chat's provider says its model takes capability `need`.
pub(crate) fn can(meta: &Map<String, Value>, need: &str) -> bool {
    entry_of(meta)
        .and_then(|e| e.get(need))
        .and_then(Value::as_bool)
        == Some(true)
}

/// The limits of the chat's model ([`Limits`]).
pub(crate) fn limits(meta: &Map<String, Value>) -> Limits {
    let empty = Map::new();
    let entry = entry_of(meta).unwrap_or(&empty);
    let own = |k: &str| {
        crate::record::image_caps::RECORD
            .get(entry, k)
            .and_then(|v| v.as_i64())
    };
    let lowered = |k: &str, setting: &str| {
        let own = own(k).expect("the capability declares its default");
        crate::record::ai::RECORD
            .get(meta, setting)
            .and_then(|v| v.as_i64())
            .map_or(own, |s| own.min(s))
    };
    Limits {
        max_images: own("max_images"),
        max_bytes: lowered("max_bytes", SETTING_BYTES),
        max_pixels: lowered("max_pixels", SETTING_PIXELS),
    }
}

/// The images a turn attached: their parts and what the row says about them.
#[derive(Clone, Debug)]
pub(crate) struct Attached {
    /// `{type, needs, ref, media_type, width, height, bytes, hash}` each.
    pub parts: Vec<Value>,
    /// `attributes.image`: `{ref, name, original, stored, tokens}` each.
    pub info: Vec<Value>,
}

impl Attached {
    /// `row` with the images in it (`images`, `attributes.image`).
    pub(crate) fn stamp(&self, mut row: Value) -> Value {
        if let Some(m) = row.as_object_mut() {
            m.insert("images".into(), Value::Array(self.parts.clone()));
            let attrs = m.entry("attributes").or_insert_with(|| json!({}));
            if let Some(a) = attrs.as_object_mut() {
                a.insert("image".into(), Value::Array(self.info.clone()));
            }
        }
        row
    }
}

/// `row` with the images of its turn, when it attached any.
pub(crate) fn stamped(row: Value, attached: Option<&Attached>) -> Value {
    match attached {
        Some(a) => a.stamp(row),
        None => row,
    }
}

/// The image parts of a stored row or message (none: `None`).
pub(crate) fn parts_of(row: &Value) -> Option<&Vec<Value>> {
    row.get("images")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
}

// ── the capability map ───────────────────────────────────────────────────────

/// Keep the chat's `provider_capabilities` right for its provider and model: the provider
/// is asked (`reflect`) at the first turn of an incarnation and whenever `provider_id` or
/// `model` changed since; the record is written only when the map is not what it holds.
/// A provider that does not answer leaves what is there (a provider that was swapped for
/// one that does not answer leaves an empty map).
pub(crate) async fn refresh(agent_id: &AgentId, kernel: &Arc<Kernel>, state: &BackendState) {
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    let Some(provider) = meta
        .get(crate::remote_provider::PROVIDER_FIELD)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let model = meta.get("model").and_then(Value::as_str).unwrap_or("");
    let key = (provider.to_string(), model.to_string());
    if state
        .caps_checked
        .lock()
        .expect("caps_checked poisoned")
        .as_ref()
        == Some(&key)
    {
        return;
    }
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(provider),
            json!({"type": "reflect"}),
        )])
        .await
        .into_iter()
        .next();
    *state.caps_checked.lock().expect("caps_checked poisoned") = Some(key);
    let held = meta.get(FIELD);
    let answered = out
        .as_ref()
        .filter(|o| {
            o.status == fantastic_kernel::Status::Completed && o.result.get("error").is_none()
        })
        .map(|o| &o.result);
    let capabilities = match answered {
        Some(r) => model_entry(r, model),
        None => {
            // not answered: what is held stays, unless it was for another provider or model
            let same = held.is_some_and(|h| {
                h.get("provider").and_then(Value::as_str) == Some(provider)
                    && h.get("model").and_then(Value::as_str) == Some(model)
            });
            if held.is_none() || same {
                return;
            }
            json!({})
        }
    };
    let new = json!({"provider": provider, "model": model, "capabilities": capabilities});
    if held != Some(&new) {
        let mut patch = Map::new();
        patch.insert(FIELD.to_string(), new);
        let _ = kernel.update(agent_id, patch);
    }
}

/// The entry of the chat's model in a provider's `reflect`: its `capabilities` map is keyed
/// by model. The chat's own `model`, else the provider's reflected `model`, else the one
/// entry the map has; any other (an unknown model) has none: an empty entry, no capability.
fn model_entry(reflect: &Value, model: &str) -> Value {
    let Some(map) = reflect.get("capabilities").and_then(Value::as_object) else {
        return json!({});
    };
    let named = Some(model).filter(|m| !m.is_empty()).or_else(|| {
        reflect
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
    });
    let found = match named {
        Some(m) => map.get(m),
        None if map.len() == 1 => map.values().next(),
        None => None,
    };
    found
        .filter(|e| e.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}))
}

// ── attach ───────────────────────────────────────────────────────────────────

/// An image prepared for the store.
struct Prepared {
    bytes: Vec<u8>,
    media_type: &'static str,
    width: u32,
    height: u32,
    /// The image as it came: width, height, bytes.
    original: (u32, u32, usize),
}

fn media_of(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

fn encode(img: &DynamicImage, format: ImageFormat) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    // a jpeg has no alpha
    let flat;
    let img = if format == ImageFormat::Jpeg {
        flat = DynamicImage::ImageRgb8(img.to_rgb8());
        &flat
    } else {
        img
    };
    img.write_to(&mut Cursor::new(&mut out), format)
        .map_err(|e| format!("cannot be re-encoded: {e}"))?;
    Ok(out)
}

/// Decode `raw` and bring it inside `limits`: kept as it is when it is, else scaled down
/// (png stays png, jpeg stays jpeg, gif and webp become png; a png that is still too large
/// in bytes becomes a jpeg) until it fits. `Err` names why: a format the crate cannot read,
/// or an image that does not fit after the last shrink.
fn prepare(raw: &[u8], limits: Limits) -> Result<Prepared, String> {
    let format = image::guess_format(raw)
        .map_err(|_| "is not an image this chat can read (png, jpeg, gif or webp)".to_string())?;
    let Some(media_type) = media_of(format) else {
        return Err(format!(
            "is a {format:?} image: this chat reads png, jpeg, gif and webp"
        ));
    };
    let img = image::load_from_memory_with_format(raw, format)
        .map_err(|e| format!("cannot be decoded: {e}"))?;
    let (w, h) = (img.width(), img.height());
    let original = (w, h, raw.len());
    let pixels = i64::from(w) * i64::from(h);
    if pixels <= limits.max_pixels && raw.len() as i64 <= limits.max_bytes {
        return Ok(Prepared {
            bytes: raw.to_vec(),
            media_type,
            width: w,
            height: h,
            original,
        });
    }
    let mut scale = if pixels > limits.max_pixels {
        (limits.max_pixels as f64 / pixels as f64).sqrt()
    } else {
        1.0
    };
    let mut out_format = if format == ImageFormat::Jpeg {
        ImageFormat::Jpeg
    } else {
        ImageFormat::Png
    };
    for _ in 0..SHRINKS {
        let (nw, nh) = (
            ((f64::from(w) * scale).floor() as u32).max(1),
            ((f64::from(h) * scale).floor() as u32).max(1),
        );
        let small = img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3);
        let bytes = encode(&small, out_format)?;
        if bytes.len() as i64 <= limits.max_bytes
            && i64::from(nw) * i64::from(nh) <= limits.max_pixels
        {
            let media_type = media_of(out_format).expect("png and jpeg have media types");
            return Ok(Prepared {
                bytes,
                media_type,
                width: nw,
                height: nh,
                original,
            });
        }
        // a png that stays too large in bytes goes on as a jpeg
        out_format = ImageFormat::Jpeg;
        scale *= 0.8;
    }
    Err(format!(
        "is {w}\u{d7}{h} and {} bytes: still over {} pixels / {} bytes after scaling it down",
        raw.len(),
        limits.max_pixels,
        limits.max_bytes
    ))
}

/// The tokens an image of w×h costs, estimated (w×h/750).
fn tokens(w: u32, h: u32) -> u64 {
    u64::from(w) * u64::from(h) / 750
}

/// Attach the images of a `send`: `specs` is its `images` list. Each is read or fetched,
/// brought inside the limits of the chat's model, and kept in the chat's store under a ref;
/// the result is what the user's row says of them. `Ok(None)`: no images. `Err` is the
/// refusal text, naming the item.
pub(crate) async fn attach(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    specs: Option<&[crate::declared::Image]>,
) -> Result<Option<Attached>, String> {
    let list = match specs {
        None | Some([]) => return Ok(None),
        Some(a) => a,
    };
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    let limits = limits(&meta);
    if can(&meta, NEEDS) && limits.max_images.is_some_and(|n| list.len() as i64 > n) {
        return Err(format!(
            "send: {} images, and this model takes {} per request",
            list.len(),
            limits.max_images.unwrap_or_default()
        ));
    }
    let mut parts = Vec::new();
    let mut info = Vec::new();
    for (i, spec) in list.iter().enumerate() {
        let at = format!("send: images[{i}]");
        let path = spec.path.as_deref().filter(|s| !s.is_empty());
        let named = spec.image_ref.as_deref().filter(|s| !s.is_empty());
        let (raw, name, from_ref) = match (path, named) {
            (Some(p), None) => {
                let bridge = meta
                    .get("file_bridge_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("{at}: a `path` is read through the chat's file_bridge_id, and the record names none"))?;
                let raw = crate::prompt_file::read_bytes(kernel, bridge, p)
                    .await
                    .map_err(|e| format!("{at}: {e}"))?;
                (raw, p.rsplit('/').next().unwrap_or(p).to_string(), None)
            }
            (None, Some(r)) => {
                let (_, raw) = crate::memory::get_bytes(agent_id, kernel, r)
                    .await
                    .map_err(|e| format!("{at}: {}", e.0))?;
                (raw, r.to_string(), Some(r))
            }
            _ => return Err(format!("{at}: name exactly one of `path` and `ref`")),
        };
        let prepared = {
            let raw = raw.clone();
            tokio::task::spawn_blocking(move || prepare(&raw, limits))
                .await
                .map_err(|e| format!("{at}: {e}"))?
                .map_err(|e| format!("{at} ({name}) {e}"))?
        };
        // bytes already in the store that need no change keep their ref
        let image_ref = match from_ref {
            Some(r) if prepared.bytes == raw => r.to_string(),
            _ => {
                let r = format!("img-{}", kernel.mint_id());
                crate::memory::put_bytes(
                    agent_id,
                    kernel,
                    &r,
                    prepared.media_type,
                    prepared.bytes.clone(),
                )
                .await
                .map_err(|e| format!("{at}: {}", e.0))?;
                r
            }
        };
        let (w, h) = (prepared.width, prepared.height);
        parts.push(json!({
            "type": "image",
            "needs": NEEDS,
            "ref": image_ref,
            "media_type": prepared.media_type,
            "width": w,
            "height": h,
            "bytes": prepared.bytes.len(),
            "hash": crate::row_attrs::short_hash_bytes(&prepared.bytes),
        }));
        let (ow, oh, ob) = prepared.original;
        info.push(json!({
            "ref": image_ref,
            "name": name,
            "original": {"width": ow, "height": oh, "bytes": ob},
            "stored": {"width": w, "height": h, "bytes": prepared.bytes.len()},
            "tokens": tokens(w, h),
        }));
    }
    Ok(Some(Attached { parts, info }))
}

// ── the forms an image row takes in a request ────────────────────────────────

/// How an image part shows in a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    /// The image itself, in the request where the model is to see it.
    Image,
    /// The image itself, brought back by `recall` (once).
    Recalled,
    /// A line naming the image, once the model has answered: `recall {id}` brings it back.
    Anchor,
    /// A line standing in for an image the model cannot be shown.
    Alt,
}

impl Form {
    /// The `form` of the request's `attributes.parts` entry.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Recalled => "image_recalled",
            Self::Anchor => "image_anchor",
            Self::Alt => "image_alt",
        }
    }
}

/// Whether one image part is inside the limits of the model.
pub(crate) fn fits(part: &Value, limits: Limits) -> bool {
    let n = |k: &str| part.get(k).and_then(Value::as_i64).unwrap_or(0);
    n("bytes") <= limits.max_bytes && n("width") * n("height") <= limits.max_pixels
}

/// The name an image goes by in a line: the one its row's `attributes.image` keeps, else its ref.
fn name_of(part: &Value, info: &[Value]) -> String {
    let image_ref = part.get("ref");
    info.iter()
        .find(|i| i.get("ref") == image_ref)
        .and_then(|i| i.get("name"))
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| image_ref.map(|r| r.as_str().map_or_else(|| r.to_string(), String::from)))
        .unwrap_or_default()
}

/// `message` as the request carries it in `form`: `Image` and `Recalled` keep the parts on
/// the message (the provider remaps them to its wire), `Anchor` and `Alt` leave the text a
/// line per image after it. The form is noted on the message as `image_form`, for the
/// request's `attributes.parts` (the render for the model drops it).
pub(crate) fn render(mut message: Value, info: &[Value], row_id: Option<i64>, form: Form) -> Value {
    let parts = parts_of(&message).cloned().unwrap_or_default();
    let lines: Vec<String> = parts
        .iter()
        .map(|p| {
            let n = |k: &str| p.get(k).and_then(Value::as_u64).unwrap_or(0);
            match form {
                Form::Anchor => format!(
                    "[image \u{b7} {}\u{d7}{} \u{b7} {} \u{b7} ~{} tokens \u{b7} id {}]",
                    n("width"),
                    n("height"),
                    p.get("media_type").and_then(Value::as_str).unwrap_or(""),
                    tokens(n("width") as u32, n("height") as u32),
                    row_id.map_or_else(String::new, |i| i.to_string()),
                ),
                _ => format!(
                    "[image: {} \u{b7} {}\u{d7}{} \u{b7} not shown to this model]",
                    name_of(p, info),
                    n("width"),
                    n("height"),
                ),
            }
        })
        .collect();
    if let Some(m) = message.as_object_mut() {
        if matches!(form, Form::Anchor | Form::Alt) {
            m.remove("images");
            let text = m.get("content").and_then(Value::as_str).unwrap_or("");
            let joined = lines.join("\n");
            let content = if text.is_empty() {
                joined
            } else {
                format!("{text}\n{joined}")
            };
            m.insert("content".into(), json!(content));
        }
        m.insert("image_form".into(), json!(form.name()));
    }
    message
}
