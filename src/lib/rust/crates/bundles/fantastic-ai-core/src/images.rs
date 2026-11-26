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
//! (`{provider, model, capabilities: {<key>: …}}`), refreshed from the provider's
//! `capabilities` verb (the chat holds `call: [complete, watch, capabilities]` on it) when it starts and whenever `provider_id` or `model` change ([`refresh`]). A part is MARKED
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

/// Keep the chat's `provider_capabilities` right for its provider and model, as a STEP
/// (#795): the provider is asked (`capabilities`) at the first turn of an incarnation and
/// whenever `provider_id` or `model` changed since, and its answer is the next step; the
/// record is written only when the map is not what it holds. A provider that does not
/// answer leaves what is there (a provider that was swapped for one that does not answer
/// leaves an empty map). `next` runs once the map is right, at once when nothing is asked.
pub(crate) async fn refresh<R, F, Fut>(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    state: Arc<BackendState>,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    let Some(provider) = meta
        .get(crate::remote_provider::PROVIDER_FIELD)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return next(rec).await;
    };
    let model = meta.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let key = (provider.clone(), model.clone());
    if state
        .caps_checked
        .lock()
        .expect("caps_checked poisoned")
        .as_ref()
        == Some(&key)
    {
        return next(rec).await;
    }
    let item = Item::new(AgentId::from(provider.as_str()), json!({"type": "capabilities"}));
    let (k, me) = (Arc::clone(kernel), agent_id.clone());
    crate::steps::call(kernel, item, rec, move |rec, out| async move {
        *state.caps_checked.lock().expect("caps_checked poisoned") = Some(key);
        took_capabilities(&me, &k, &state, &meta, &provider, &model, &out).await;
        next(rec).await;
    })
    .await;
}

/// The provider's `capabilities` answer `out`, taken: the model it names noted, and the
/// chat's `provider_capabilities` written when it changed.
async fn took_capabilities(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    state: &BackendState,
    meta: &Map<String, Value>,
    provider: &str,
    model: &str,
    out: &fantastic_kernel::Outcome,
) {
    let held = meta.get(FIELD);
    let answered = Some(out)
        .filter(|o| {
            o.status == fantastic_kernel::Status::Completed && o.result.get("error").is_none()
        })
        .map(|o| &o.result);
    // the model the provider names is a source of the chat's call format (#766)
    if let Some(m) = answered
        .and_then(|r| r.get("model"))
        .and_then(Value::as_str)
    {
        crate::model_format::rules::note_seen(state, m);
    }
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
        // the chat's own field, written as itself
        let _ = kernel.write_own(agent_id, patch).await;
    }
}

/// The entry of the chat's model in a provider's `capabilities` answer: its `capabilities`
/// map is keyed by model. The chat's own `model`, else the provider's `model`, else the one
/// entry the map has; any other (an unknown model) has none: an empty entry, no capability.
fn model_entry(answer: &Value, model: &str) -> Value {
    let Some(map) = answer.get("capabilities").and_then(Value::as_object) else {
        return json!({});
    };
    let named = Some(model).filter(|m| !m.is_empty()).or_else(|| {
        answer
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

/// What the next step of an attach gets: what the row says of the images, `None` for
/// none, or the refusal text naming the item.
pub(crate) type AttachResult = Result<Option<Attached>, String>;

/// Attach the images of a `send`, as STEPS (#795): `specs` is its `images` list. Each is
/// read (the chat's file bridge) or fetched (its store), brought inside the limits of the
/// chat's model, and kept in the chat's store under a ref, one call per step; the next
/// step gets what the user's row says of them (`Ok(None)`: no images), or the refusal
/// text naming the item.
pub(crate) async fn attach<R, F, Fut>(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    specs: Option<Vec<crate::declared::Image>>,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R, AttachResult) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let list = match specs {
        Some(a) if !a.is_empty() => a,
        _ => return next(rec, Ok(None)).await,
    };
    let meta = crate::helpers::agent_meta(agent_id, kernel);
    let limits = limits(&meta);
    if can(&meta, NEEDS) && limits.max_images.is_some_and(|n| list.len() as i64 > n) {
        let why = format!(
            "send: {} images, and this model takes {} per request",
            list.len(),
            limits.max_images.unwrap_or_default()
        );
        return next(rec, Err(why)).await;
    }
    let a = Attaching {
        agent: agent_id.clone(),
        kernel: Arc::clone(kernel),
        meta,
        limits,
        list,
        i: 0,
        parts: Vec::new(),
        info: Vec::new(),
        next: Box::new(move |r| Box::pin(next(rec, r))),
    };
    a.step().await;
}

/// An attach in flight: the images, the one being taken, what is attached so far, and
/// the step that gets the result.
struct Attaching {
    agent: AgentId,
    kernel: Arc<Kernel>,
    meta: Map<String, Value>,
    limits: Limits,
    list: Vec<crate::declared::Image>,
    i: usize,
    parts: Vec<Value>,
    info: Vec<Value>,
    next: Box<dyn FnOnce(AttachResult) -> futures_util::future::BoxFuture<'static, ()> + Send>,
}

impl Attaching {
    fn at(&self) -> String {
        format!("send: images[{}]", self.i)
    }

    /// The attach ends: the next step gets `r`.
    async fn end(self, r: AttachResult) {
        (self.next)(r).await;
    }

    /// TAKE IMAGE `i`: its bytes read (a step), or the attach ends with every image.
    fn step(self) -> futures_util::future::BoxFuture<'static, ()> {
        Box::pin(async move {
            let Some(spec) = self.list.get(self.i).cloned() else {
                let r = Ok(Some(Attached { parts: self.parts.clone(), info: self.info.clone() }));
                return self.end(r).await;
            };
            let at = self.at();
            let path = spec.path.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
            let named = spec.image_ref.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
            match (path, named) {
                (Some(p), None) => {
                    let Some(bridge) = self
                        .meta
                        .get("file_bridge_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                    else {
                        let why = format!("{at}: a `path` is read through the chat's file_bridge_id, and the record names none");
                        return self.end(Err(why)).await;
                    };
                    let name = p.rsplit('/').next().unwrap_or(&p).to_string();
                    let kernel = Arc::clone(&self.kernel);
                    crate::prompt_file::read_bytes(&kernel, bridge, p, self, move |a, r| async move {
                        match r {
                            Ok(raw) => a.prepared(raw, name, None).await,
                            Err(e) => {
                                let why = format!("{}: {e}", a.at());
                                a.end(Err(why)).await;
                            }
                        }
                    })
                    .await;
                }
                (None, Some(r)) => {
                    let (agent, kernel) = (self.agent.clone(), Arc::clone(&self.kernel));
                    let payload = json!({"type": "get_bytes", "ref": r});
                    crate::steps::store(&agent, &kernel, payload, None, self, move |a, got| async move {
                        let raw = got.and_then(|(_, body)| {
                            body.ok_or_else(|| {
                                crate::memory::MemoryStoreError(format!(
                                    "{}: memory store returned no bytes for image `{r}`",
                                    a.agent
                                ))
                            })
                        });
                        match raw {
                            Ok(raw) => a.prepared(raw, r.clone(), Some(r)).await,
                            Err(e) => {
                                let why = format!("{}: {}", a.at(), e.0);
                                a.end(Err(why)).await;
                            }
                        }
                    })
                    .await;
                }
                _ => {
                    let why = format!("{at}: name exactly one of `path` and `ref`");
                    self.end(Err(why)).await;
                }
            }
        })
    }

    /// Image `i`'s bytes `raw`, brought inside the limits (the agent's own work); kept in
    /// the store under a new ref (a step) unless they are unchanged bytes of a ref.
    async fn prepared(self, raw: Vec<u8>, name: String, from_ref: Option<String>) {
        let at = self.at();
        let limits = self.limits;
        let prepared = {
            let raw = raw.clone();
            match tokio::task::spawn_blocking(move || prepare(&raw, limits)).await {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => return self.end(Err(format!("{at} ({name}) {e}"))).await,
                Err(e) => return self.end(Err(format!("{at}: {e}"))).await,
            }
        };
        // bytes already in the store that need no change keep their ref
        match from_ref {
            Some(r) if prepared.bytes == raw => self.kept(prepared, name, r).await,
            _ => {
                let r = format!("img-{}", self.kernel.mint_id());
                let head = json!({"type": "put_bytes", "ref": r, "media_type": prepared.media_type});
                let (agent, kernel) = (self.agent.clone(), Arc::clone(&self.kernel));
                let body = prepared.bytes.clone();
                crate::steps::store(&agent, &kernel, head, Some(body), (self, prepared), move |(a, prepared), put| async move {
                    match put {
                        Ok(_) => a.kept(prepared, name, r).await,
                        Err(e) => {
                            let why = format!("{}: {}", a.at(), e.0);
                            a.end(Err(why)).await;
                        }
                    }
                })
                .await;
            }
        }
    }

    /// Image `i` is kept under `image_ref`: what the row says of it, then the next image.
    async fn kept(mut self, prepared: Prepared, name: String, image_ref: String) {
        let (w, h) = (prepared.width, prepared.height);
        self.parts.push(json!({
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
        self.info.push(json!({
            "ref": image_ref,
            "name": name,
            "original": {"width": ow, "height": oh, "bytes": ob},
            "stored": {"width": w, "height": h, "bytes": prepared.bytes.len()},
            "tokens": tokens(w, h),
        }));
        self.i += 1;
        self.step().await;
    }
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
/// the message (the provider remaps them to its wire), `Anchor` and `Alt` leave a
/// [`Piece::Image`] per image after its text (the type's line for it is written by the
/// map). The form is noted on the message as `image_form`, for the request's
/// `attributes.parts`.
pub(crate) fn render(mut message: Value, info: &[Value], row_id: Option<i64>, form: Form) -> Value {
    use crate::model_format::map::{pieces_of, with_pieces, ImageForm, Piece};
    let parts = parts_of(&message).cloned().unwrap_or_default();
    let shown = match form {
        Form::Anchor => Some(ImageForm::Anchor),
        Form::Alt => Some(ImageForm::Alt),
        Form::Image | Form::Recalled => None,
    };
    if let Some(how) = shown {
        let mut pieces = pieces_of(&message);
        pieces.extend(parts.iter().map(|p| {
            let n = |k: &str| p.get(k).and_then(Value::as_u64).unwrap_or(0);
            Piece::Image {
                form: how,
                id: row_id,
                name: name_of(p, info),
                width: n("width"),
                height: n("height"),
                media_type: p
                    .get("media_type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                tokens: tokens(n("width") as u32, n("height") as u32),
            }
        }));
        if let Some(m) = message.as_object_mut() {
            m.remove("images");
        }
        message = with_pieces(message, &pieces);
    }
    if let Some(m) = message.as_object_mut() {
        m.insert("image_form".into(), json!(form.name()));
    }
    message
}
