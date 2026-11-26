//! THE RENDER CACHE (#766, spec 1.6): each chat's own, in its RAM state only.
//!
//! A pass maps each message of its view for the model type ([`super::map`], from the
//! type's [`super::Decl`]). A stored row's mapped text is kept under `(type, row id)`
//! with the source it was mapped from — the canonical bytes of what it shows
//! ([`super::map::canonical`]) — so a warm pass maps only its new and changed rows.
//! Nothing of it is stored or sent; it goes with the chat's state
//! ([`crate::state::drop_state`]).
//!
//! What it borrows from vLLM's automatic prefix caching (design, not code): the chained
//! key (each link covers its parent), a collision-safe hash (SHA-256) checked by a byte
//! compare, pin counts with an LRU list where an unpinned entry stays usable until it is
//! evicted, no caching of the message still in progress, and a reset that clears
//! everything at once.
//!
//! - **Entries.** Key `(type, row id)`. A hit needs an equal digest and then equal bytes
//!   (the byte compare is the truth: an equal digest over other bytes is a collision,
//!   counted, and a miss). A hit runs no adapter code. Every type keeps entries.
//! - **Never cached:** the system message, the arriving message (the last), a message
//!   with no `id` or with non-string content. They are rendered every time.
//! - **Drops.** A row whose source changed is rendered again and its drop labelled by
//!   [`why`]; a row of the type that is not in the view is dropped as [`Why::Cut`] when
//!   the pass finishes. Nothing else drops an entry.
//! - **Chain.** Per type, the links of the last finished pass. `kept` = the leading
//!   messages whose links are equal: the prefix this pass sends unchanged.
//!   `kept_endpoint` = `kept`, but 0 when the model id or the provider differs from the
//!   last finished pass's, or the endpoint's cache is past [`CACHE_LIFETIME_SECS`]. Both
//!   are statements about the chat's own bytes, never measurements of the endpoint.
//! - **Bound, LRU, pins.** The chat's bound ([`bound_of`], read per pass) counts the
//!   source and rendered bytes. A pass pins every entry it uses; its [`View`] holds the pins and
//!   releases them when dropped or finished. Over the bound the least recently used
//!   unpinned entries go; a pinned one never does, so the bound may be exceeded until
//!   the pass ends.
//! - **Epoch.** `H(rule file bytes, each type's name, rev, renderer rev and every
//!   resource text it maps with)`, checked at every pass: another epoch clears every
//!   entry and chain, so an edited text never serves stale bytes.
//! - **Concurrency.** One mutex, taken per lookup and per insert, never across a
//!   `render`. Two lookups that miss one key may both render: the second insert replaces
//!   the first with equal bytes and the byte count stays exact.
//!
//! Every map path here is a pure function of (source, type, rev): no clock, no
//! environment, no random value, no unordered map. The clock feeds `kept_endpoint` only.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde_json::{json, Map, Value};
use sha2::{Digest as _, Sha256};

use super::map::{canonical, Shown};
use super::{trace, Decl, Section};
use crate::constants::CACHE_LIFETIME_SECS;

/// A SHA-256 sized digest.
pub(crate) type Hash = [u8; 32];

/// The digest of an entry's source. A type parameter so a test can force a collision
/// (K20, K21); [`Sha`] everywhere else. The chain's links are always SHA-256.
pub(crate) trait Hasher: Send + Sync + 'static {
    /// The digest of `bytes`.
    fn digest(bytes: &[u8]) -> Hash;
}

/// SHA-256: the cache's hasher.
pub(crate) struct Sha;

impl Hasher for Sha {
    fn digest(bytes: &[u8]) -> Hash {
        Sha256::digest(bytes).into()
    }
}

/// SHA-256 over `parts`, each prefixed by its length, so no two part lists collide by
/// where one part ends.
fn sha(parts: &[&[u8]]) -> Hash {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize().into()
}

/// The epoch of a build: the rule file's bytes and, for each type, its name, its rev,
/// its call renderer's rev, and every resource text it maps with (sections, markers).
pub(crate) fn epoch_of(rules: &[u8], decls: &[&Decl<'_>]) -> Hash {
    let mut h = Sha256::new();
    let mut part = |b: &[u8]| {
        h.update((b.len() as u64).to_le_bytes());
        h.update(b);
    };
    part(rules);
    for d in decls {
        part(d.name.as_bytes());
        part(&d.rev.to_le_bytes());
        part(d.calls.name().as_bytes());
        part(&d.calls.rev().to_le_bytes());
        for s in Section::ALL {
            part((d.texts)(s).as_bytes());
        }
        for (_, t) in d.markers.all() {
            part(t.as_bytes());
        }
    }
    h.finalize().into()
}

/// The epoch of this binary: the shipped rule file and every registered type.
pub(crate) fn epoch() -> Hash {
    static EPOCH: OnceLock<Hash> = OnceLock::new();
    *EPOCH.get_or_init(|| {
        let decls: Vec<&Decl<'_>> = super::REGISTRY.iter().collect();
        epoch_of(super::rules::RULES_JSON.as_bytes(), &decls)
    })
}

/// The record field that bounds a chat's render cache, in bytes.
pub(crate) const BOUND_FIELD: &str = "render_cache_bytes";

/// The chat's bound: its record's `render_cache_bytes` (a meta bundle's `record:` under
/// it), else the declared default, [`crate::constants::RENDER_CACHE_BYTES`]. A value that
/// is not an integer of at least 1 reads as the default; a record write refuses one.
#[must_use]
pub(crate) fn bound_of(meta: &Map<String, Value>) -> usize {
    let bytes = |v: Option<Value>| {
        v.and_then(|v| v.as_u64())
            .filter(|n| *n >= 1)
            .and_then(|n| usize::try_from(n).ok())
    };
    // a null field matches the declared type `integer | null`: it is the default too
    bytes(crate::chat_bundle::RECORD.get(meta, BOUND_FIELD))
        .or_else(|| bytes(crate::chat_bundle::RECORD.get(&Map::new(), BOUND_FIELD)))
        .expect("render_cache_bytes declares its default")
}

/// Why an entry was dropped: the `why` of a `cache_dropped` trace row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// The row is shown at a lower rank (or another reduced text).
    Fold,
    /// The row is not in the view.
    Cut,
    /// The row is shown at a higher rank, or recalled.
    Recall,
    /// The row's full text changed with no recall: no known path, never a wrong reason.
    Changed,
}

impl Why {
    /// The trace value.
    #[must_use]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Fold => "fold",
            Self::Cut => "cut",
            Self::Recall => "recall",
            Self::Changed => "changed",
        }
    }
}

/// A form's rank: full > line > reduced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Reduced,
    Line,
    Full,
}

/// The rank of a row's `form` (`request_parts`). A row with no form is shown whole.
fn rank(form: Option<&str>) -> Rank {
    match form {
        Some("anchor" | "stub" | "image_alt") => Rank::Reduced,
        Some("image_anchor") => Rank::Line,
        _ => Rank::Full,
    }
}

/// The `why` of a drop whose source bytes differ: from the entry's form (`was`) and this
/// pass's (`now`), by the table of spec 1.6.
#[must_use]
pub(crate) fn why(was: Option<&str>, now: Option<&str>) -> Why {
    let (a, b) = (rank(was), rank(now));
    if a > b {
        Why::Fold
    } else if a < b {
        Why::Recall
    } else if a == Rank::Reduced {
        Why::Fold
    } else if matches!(now, Some("recalled" | "image_recalled")) {
        Why::Recall
    } else {
        Why::Changed
    }
}

/// What a pass hands the cache beside its messages.
pub(crate) struct Ctx<'a> {
    /// The build's epoch ([`epoch`]).
    pub(crate) epoch: Hash,
    /// The chat's bound for this pass ([`bound_of`]).
    pub(crate) bound: usize,
    /// The model id of the pass.
    pub(crate) model: Option<&'a str>,
    /// The chat's `provider_id`.
    pub(crate) provider_id: Option<&'a str>,
    /// When the chat's last provider pass ended (`last_send_secs`).
    pub(crate) last_send: Option<f64>,
}

type Key = (&'static str, i64);

/// One row's mapped text for one type.
struct Entry {
    /// What the row shows, canonical ([`canonical`]).
    source: Vec<u8>,
    /// The source's digest.
    digest: Hash,
    /// The source mapped for the type.
    rendered: String,
    /// The row's form when the entry was last used.
    form: Option<String>,
    /// Passes using the entry now.
    pins: u32,
    /// The cache's use counter when it was last used.
    used: u64,
    /// The number of the pass that last used it.
    #[cfg_attr(not(test), allow(dead_code))]
    pass: u64,
}

impl Entry {
    fn size(&self) -> usize {
        self.source.len() + self.rendered.len()
    }
}

/// What the last finished pass was sent to.
#[derive(Clone, PartialEq, Eq)]
struct Last {
    model: Option<String>,
    provider_id: Option<String>,
}

#[derive(Default)]
struct Inner {
    epoch: Option<Hash>,
    entries: BTreeMap<Key, Entry>,
    /// The unpinned entries by last use: the LRU list.
    free: BTreeMap<u64, Key>,
    tick: u64,
    bytes: usize,
    collisions: u64,
    passes: u64,
    /// Per type, the links of its last finished pass.
    chains: BTreeMap<&'static str, Vec<Hash>>,
    last: Option<Last>,
}

impl Inner {
    fn clear(&mut self) {
        self.entries.clear();
        self.free.clear();
        self.chains.clear();
        self.bytes = 0;
        self.last = None;
    }

    /// A new use of `key`: it moves to the LRU list's end.
    fn touch(&mut self, key: Key, pass: u64) {
        self.tick += 1;
        let tick = self.tick;
        let Some(e) = self.entries.get_mut(&key) else {
            return;
        };
        if e.pins == 0 {
            self.free.remove(&e.used);
            self.free.insert(tick, key);
        }
        e.used = tick;
        e.pass = pass;
    }

    fn pin(&mut self, key: Key) {
        if let Some(e) = self.entries.get_mut(&key) {
            if e.pins == 0 {
                self.free.remove(&e.used);
            }
            e.pins += 1;
        }
    }

    fn unpin(&mut self, key: Key) {
        if let Some(e) = self.entries.get_mut(&key) {
            if e.pins > 0 {
                e.pins -= 1;
                if e.pins == 0 {
                    self.free.insert(e.used, key);
                }
            }
        }
    }

    fn remove(&mut self, key: Key) -> Option<Entry> {
        let e = self.entries.remove(&key)?;
        if e.pins == 0 {
            self.free.remove(&e.used);
        }
        self.bytes -= e.size();
        Some(e)
    }

    /// Put `entry` under `key`, pinned once by the inserting pass. An entry already
    /// there (another lookup's insert) is replaced, and its pins are kept.
    fn insert(&mut self, key: Key, mut entry: Entry, pass: u64) {
        let held = self.remove(key).map_or(0, |old| old.pins);
        entry.pins = held + 1;
        self.bytes += entry.size();
        self.entries.insert(key, entry);
        self.touch(key, pass);
    }

    /// The least recently used unpinned entries go until the bytes are within `bound`.
    fn evict(&mut self, bound: usize) {
        while self.bytes > bound {
            let Some((_, key)) = self.free.pop_first() else {
                return;
            };
            if let Some(e) = self.entries.remove(&key) {
                self.bytes -= e.size();
            }
        }
    }

    /// [`Inner::evict`] during a pass: an entry of `kind` whose row is in the pass's view
    /// is spared, as the pass will pin it when it reaches that row.
    fn evict_sparing(&mut self, bound: usize, kind: &'static str, view: &BTreeSet<i64>) {
        if self.bytes <= bound {
            return;
        }
        let victims: Vec<(u64, Key)> = self
            .free
            .iter()
            .filter(|(_, k)| !(k.0 == kind && view.contains(&k.1)))
            .map(|(u, k)| (*u, *k))
            .collect();
        for (used, key) in victims {
            if self.bytes <= bound {
                return;
            }
            self.free.remove(&used);
            if let Some(e) = self.entries.remove(&key) {
                self.bytes -= e.size();
            }
        }
    }
}

/// A chat's render cache. `H` digests sources ([`Sha`]; a test forces collisions).
pub(crate) struct RenderCache<H: Hasher = Sha> {
    inner: Mutex<Inner>,
    /// Unix seconds now: read for `kept_endpoint` only, never on a render path.
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    hasher: PhantomData<fn() -> H>,
}

impl<H: Hasher> Default for RenderCache<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: Hasher> RenderCache<H> {
    /// An empty cache on the wall clock.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::with_clock(crate::helpers::now_secs)
    }

    /// An empty cache whose `kept_endpoint` reads `clock` (C14).
    #[must_use]
    pub(crate) fn with_clock(clock: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            clock: Box::new(clock),
            hasher: PhantomData,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // a panic elsewhere leaves nothing half-written that a later pass could read
        // wrong: every entry is checked by its bytes
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The view of one pass, for `decl`'s type: each message of `shown` mapped for the
    /// chat `own` ([`super::map::message`]); `role`, `images` and `cache_mark` are carried
    /// unchanged. The returned [`View`] holds the pass's pins until it is finished or
    /// dropped.
    #[cfg(test)]
    pub(crate) fn view<'c>(
        &'c self,
        decl: &Decl<'_>,
        own: &str,
        shown: &[Shown],
        ctx: &Ctx<'_>,
    ) -> View<'c, H> {
        Self::build(self, CacheRef::Borrowed(self), decl, own, shown, ctx)
    }

    /// [`Self::view`] holding the cache SHARED: a view that outlives the step that made
    /// it (a pass waits for its request's settle as a message, #795).
    pub(crate) fn view_shared(
        self: &Arc<Self>,
        decl: &Decl<'_>,
        own: &str,
        shown: &[Shown],
        ctx: &Ctx<'_>,
    ) -> View<'static, H>
    where
        H: 'static,
    {
        Self::build(&Arc::clone(self), CacheRef::Shared(Arc::clone(self), PhantomData), decl, own, shown, ctx)
    }

    fn build<'c>(
        this: &Self,
        cache: CacheRef<'c, H>,
        decl: &Decl<'_>,
        own: &str,
        shown: &[Shown],
        ctx: &Ctx<'_>,
    ) -> View<'c, H> {
        let kind = decl.name;
        let (pass, kept_chain, last) = {
            let mut inner = this.lock();
            if inner.epoch != Some(ctx.epoch) {
                inner.clear();
                inner.epoch = Some(ctx.epoch);
            }
            inner.passes += 1;
            (
                inner.passes,
                inner.chains.get(kind).cloned(),
                inner.last.clone(),
            )
        };
        let mut view = View {
            cache,
            kind,
            messages: Vec::with_capacity(shown.len()),
            rows: 0,
            kept: 0,
            kept_endpoint: 0,
            cold: true,
            dropped: Vec::new(),
            links: Vec::with_capacity(shown.len()),
            in_view: BTreeSet::new(),
            pins: Vec::new(),
            bound: ctx.bound,
            model: ctx.model.map(str::to_string),
            provider_id: ctx.provider_id.map(str::to_string),
        };
        let mut hits = 0usize;
        let arriving = shown.len().saturating_sub(1);
        view.in_view = shown.iter().filter_map(|s| s.id).collect();
        for (i, s) in shown.iter().enumerate() {
            let form = s.form.as_deref();
            let cacheable = i != arriving && s.role != "system" && s.is_text();
            let content = match (cacheable, s.id) {
                (true, Some(id)) => {
                    let (rendered, hit) =
                        this.row(decl, own, &mut view, (kind, id), s, form, pass, ctx.bound);
                    hits += usize::from(hit);
                    view.rows += usize::from(!hit);
                    Value::String(rendered)
                }
                (_, id) => {
                    // an entry for a row that can no longer be cached is dropped
                    if let Some(id) = id {
                        let mut inner = this.lock();
                        let w = inner
                            .entries
                            .get(&(kind, id))
                            .filter(|e| e.pins == 0)
                            .map(|old| why(old.form.as_deref(), form));
                        if let Some(w) = w {
                            inner.remove((kind, id));
                            view.dropped.push((id, w));
                        }
                    }
                    super::map::content(decl, own, s)
                }
            };
            view.messages.push(super::map::message_with(s, content));
        }
        // THE CHAIN, over the mapped view
        let epoch = ctx.epoch;
        for (i, m) in view.messages.iter().enumerate() {
            let digest = message_digest(m);
            let link = match view.links.last() {
                None => sha(&[&epoch, kind.as_bytes(), &digest]),
                Some(prev) => {
                    let id = shown.get(i).and_then(|s| s.id).unwrap_or(0);
                    sha(&[prev, &id.to_le_bytes(), &digest])
                }
            };
            view.links.push(link);
        }
        view.kept = kept_chain.as_ref().map_or(0, |k| {
            k.iter()
                .zip(&view.links)
                .take_while(|(a, b)| a == b)
                .count()
        });
        let same_endpoint = last.is_some_and(|l| {
            l == Last {
                model: view.model.clone(),
                provider_id: view.provider_id.clone(),
            }
        });
        let warm = ctx
            .last_send
            .is_some_and(|t| (this.clock)() - t <= CACHE_LIFETIME_SECS);
        view.kept_endpoint = if same_endpoint && warm { view.kept } else { 0 };
        view.cold = hits == 0;
        view
    }

    /// One cacheable row: its mapped text, and whether it was a hit.
    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        decl: &Decl<'_>,
        own: &str,
        view: &mut View<'_, H>,
        key: Key,
        shown: &Shown,
        form: Option<&str>,
        pass: u64,
        bound: usize,
    ) -> (String, bool) {
        let source = canonical(shown);
        let digest = H::digest(&source);
        {
            let mut inner = self.lock();
            let found = inner
                .entries
                .get(&key)
                .map(|e| (e.digest == digest, e.source == source, e.form.clone()));
            match found {
                Some((true, true, _)) => {
                    let e = inner.entries.get_mut(&key).expect("found");
                    e.form = form.map(str::to_string);
                    let rendered = e.rendered.clone();
                    inner.pin(key);
                    inner.touch(key, pass);
                    view.pins.push(key);
                    return (rendered, true);
                }
                Some((same_digest, _, was)) => {
                    if same_digest {
                        inner.collisions += 1;
                    }
                    view.dropped.push((key.1, why(was.as_deref(), form)));
                    inner.remove(key);
                }
                None => {}
            }
        }
        // mapped with no lock held
        let rendered = match super::map::content(decl, own, shown) {
            Value::String(s) => s,
            other => other.to_string(),
        };
        let mut inner = self.lock();
        inner.insert(
            key,
            Entry {
                source,
                digest,
                rendered: rendered.clone(),
                form: form.map(str::to_string),
                pins: 0,
                used: 0,
                pass,
            },
            pass,
        );
        view.pins.push(key);
        inner.evict_sparing(bound, view.kind, &view.in_view);
        (rendered, false)
    }

    /// What the cache holds now (tests).
    #[cfg(test)]
    pub(crate) fn stats(&self) -> Stats {
        let inner = self.lock();
        Stats {
            entries: inner.entries.len(),
            bytes: inner.bytes,
            counted: inner.entries.values().map(Entry::size).sum(),
            pins: inner.entries.values().map(|e| e.pins as usize).sum(),
            collisions: inner.collisions,
            chains: inner.chains.len(),
            keys: inner
                .entries
                .keys()
                .map(|(t, id)| ((*t).to_string(), *id))
                .collect(),
            last_pass: inner.entries.values().map(|e| e.pass).collect(),
            passes: inner.passes,
        }
    }

    /// The rendered text kept for `(kind, id)` (tests).
    #[cfg(test)]
    pub(crate) fn rendered(&self, kind: &str, id: i64) -> Option<String> {
        self.lock()
            .entries
            .iter()
            .find(|((t, i), _)| *t == kind && *i == id)
            .map(|(_, e)| e.rendered.clone())
    }

    /// The kept chain of `kind` (tests).
    #[cfg(test)]
    pub(crate) fn chain(&self, kind: &str) -> Option<Vec<Hash>> {
        self.lock()
            .chains
            .iter()
            .find(|(t, _)| **t == kind)
            .map(|(_, c)| c.clone())
    }
}

/// What a cache holds (tests).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stats {
    /// Entries, every type.
    pub(crate) entries: usize,
    /// The byte count the cache keeps.
    pub(crate) bytes: usize,
    /// The bytes summed over the entries now.
    pub(crate) counted: usize,
    /// Pins held, summed.
    pub(crate) pins: usize,
    /// Digest collisions seen.
    pub(crate) collisions: u64,
    /// Kept chains.
    pub(crate) chains: usize,
    /// Every key `(type, row id)`, in order.
    pub(crate) keys: Vec<(String, i64)>,
    /// The pass that last used each entry, in the order of `keys`.
    pub(crate) last_pass: Vec<u64>,
    /// Passes begun on this cache.
    pub(crate) passes: u64,
}

/// The digest of one mapped message: its role and its content.
fn message_digest(m: &Value) -> Hash {
    let role = m.get("role").and_then(Value::as_str).unwrap_or("");
    let content = match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    sha(&[role.as_bytes(), content.as_bytes()])
}

/// How a view reaches its cache: borrowed for one step, or shared by a pass whose
/// view outlives the step that made it.
pub(crate) enum CacheRef<'c, H: Hasher> {
    /// Borrowed (tests).
    #[cfg(test)]
    Borrowed(&'c RenderCache<H>),
    /// Shared.
    Shared(Arc<RenderCache<H>>, PhantomData<&'c ()>),
}

impl<H: Hasher> std::ops::Deref for CacheRef<'_, H> {
    type Target = RenderCache<H>;
    fn deref(&self) -> &RenderCache<H> {
        match self {
            #[cfg(test)]
            Self::Borrowed(c) => c,
            Self::Shared(c, _) => c,
        }
    }
}

/// One pass's view, with the pins it holds. [`View::finish`] ends the pass; a view
/// dropped unfinished (a cancel, a failure, a rerun) releases its pins and keeps the
/// chain of the last finished pass.
pub(crate) struct View<'c, H: Hasher = Sha> {
    cache: CacheRef<'c, H>,
    kind: &'static str,
    /// The messages to send: role, mapped content, images and cache mark.
    pub(crate) messages: Vec<Value>,
    /// Rows rendered now (inserts and drops).
    pub(crate) rows: usize,
    /// Leading messages whose links equal the kept chain's.
    pub(crate) kept: usize,
    /// `kept`, or 0 when the endpoint's cache cannot hold it.
    pub(crate) kept_endpoint: usize,
    /// No entry of this type was hit.
    pub(crate) cold: bool,
    /// The drops so far, in order: `(row id, why)`.
    pub(crate) dropped: Vec<(i64, Why)>,
    /// The chain of this pass.
    pub(crate) links: Vec<Hash>,
    in_view: BTreeSet<i64>,
    pins: Vec<Key>,
    bound: usize,
    model: Option<String>,
    provider_id: Option<String>,
}

impl<H: Hasher> View<'_, H> {
    /// The pass ended: entries of the type whose row is not in the view are dropped as
    /// cut, the chain is kept as the type's, the pins are released and the bound met.
    /// Its `render_cold`/`render_warm` and `cache_dropped` rows go to `events`. The drops,
    /// in order.
    pub(crate) fn finish(
        mut self,
        events: &mut trace::Events,
        mode: &'static str,
    ) -> Vec<(i64, Why)> {
        {
            let mut inner = self.cache.lock();
            let gone: Vec<Key> = inner
                .entries
                .range((self.kind, i64::MIN)..=(self.kind, i64::MAX))
                .filter(|((_, id), e)| !self.in_view.contains(id) && e.pins == 0)
                .map(|(k, _)| *k)
                .collect();
            for key in gone {
                inner.remove(key);
                self.dropped.push((key.1, Why::Cut));
            }
            inner
                .chains
                .insert(self.kind, std::mem::take(&mut self.links));
            inner.last = Some(Last {
                model: self.model.take(),
                provider_id: self.provider_id.take(),
            });
            for key in std::mem::take(&mut self.pins) {
                inner.unpin(key);
            }
            inner.evict(self.bound);
        }
        let mut fields = Map::new();
        fields.insert("rows".into(), json!(self.rows));
        fields.insert("kept".into(), json!(self.kept));
        fields.insert("kept_endpoint".into(), json!(self.kept_endpoint));
        let event = if self.cold {
            "render_cold"
        } else {
            "render_warm"
        };
        events.push(mode, self.kind, event, fields);
        for (row, w) in &self.dropped {
            let mut fields = Map::new();
            fields.insert("row".into(), json!(row));
            fields.insert("why".into(), json!(w.name()));
            events.push(mode, self.kind, "cache_dropped", fields);
        }
        std::mem::take(&mut self.dropped)
    }
}

impl<H: Hasher> Drop for View<'_, H> {
    fn drop(&mut self) {
        if self.pins.is_empty() {
            return;
        }
        let mut inner = self.cache.lock();
        for key in std::mem::take(&mut self.pins) {
            inner.unpin(key);
        }
        inner.evict(self.bound);
    }
}
