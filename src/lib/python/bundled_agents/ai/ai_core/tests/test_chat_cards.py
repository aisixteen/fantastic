"""Inline agent views in a chat (#339): `embed {id}` / `unembed {id}`. Port of the Rust
#340 tests (tests_embed.rs). Cards are store rows with no role: they never reach the
prompt, they show in `history` (digest kind `embed`), and a restart redraws them."""

from __future__ import annotations

from pathlib import Path

from ai_core import constants, core, embed
from ai_core.core import Backend, _mem
from _fake import FakeCtx

SELF = "ai"
ANTHROPIC_README = (
    Path(__file__).resolve().parents[2]
    / "anthropic"
    / "anthropic_backend"
    / "src"
    / "anthropic_backend"
    / "readme.md"
)

GOOD_VIEW = {"kind": "html", "bytes_file": "panel.html", "calls": ["get_html", "watch"]}
GOOD = {"view": GOOD_VIEW, "callable_by": {"user": ["get_html", "watch"]}}
WEAK = {
    "view": GOOD_VIEW,
    "callable_by": {"user": ["watch"]},
}
BARE = {"name": "bare", "callable_by": {"user": ["get_html", "watch"]}}


class PanelCtx(FakeCtx):
    """FakeCtx with per-id agent records: the host agents a chat can embed."""

    def __init__(self, records: dict):
        super().__init__()
        self.records = {SELF: self.record, **records}

    def get(self, _id):
        return self.records.get(_id)


def _cfg() -> Backend:
    return Backend(sentence="s", make_provider=lambda *a: None, name="fake")


async def _history(k, payload: dict | None = None) -> dict:
    return await core._make_history(_cfg())(SELF, payload or {}, k)


def _cards(rows: list[dict]) -> list[str]:
    return embed.cards(rows)


def _card_rows(k) -> list[dict]:
    return [r for r in k.rows_of(SELF) if r.get("kind") in ("embed", "unembed")]


def test_the_chat_card_cap_is_1200():
    assert constants.CHAT_CARD_MAX_HEIGHT == 1200


async def test_embed_stores_a_row_and_history_replays_it():
    k = PanelCtx({"panel": GOOD})
    res = await embed.embed(SELF, {"id": "panel"}, k)
    assert res["embedded"] == "panel"
    assert res["card"] == {"width": "100%", "max_height": 1200}

    rows = [r for r in k.rows_of(SELF) if r.get("kind") == "embed"]
    assert len(rows) == 1
    assert rows[0]["content"] == "panel"
    assert rows[0].get("role") is None

    h = await _history(k)
    digests = [d for d in h["rows"] if d["kind"] == "embed"]
    assert len(digests) == 1
    assert digests[0]["head"] == "panel"
    assert digests[0]["id"] == rows[0]["id"]
    assert h["total"] == len(h["rows"])
    assert _cards(h["rows"]) == ["panel"]

    # a restart: the RAM head is gone, the store still holds the card
    _mem(k)["heads"].clear()
    h2 = await _history(k)
    assert _cards(h2["rows"]) == ["panel"]

    again = await embed.embed(SELF, {"id": "panel"}, k)
    assert again == {
        "embedded": "panel",
        "already": True,
        "card": {"width": "100%", "max_height": 1200},
    }
    assert len([r for r in k.rows_of(SELF) if r.get("kind") == "embed"]) == 1


async def test_embed_refuses_missing_view_missing_agent_and_missing_grants():
    k = PanelCtx({"panel": GOOD, "bare": BARE, "weak": WEAK})
    before = len(k.rows_of(SELF))

    nf = await embed.embed(SELF, {"id": "nobody"}, k)
    assert nf["reason"] == "not_found"

    nv = await embed.embed(SELF, {"id": "bare"}, k)
    assert nv["reason"] == "no_view"

    gm = await embed.embed(SELF, {"id": "weak"}, k)
    assert gm["reason"] == "grants_missing"
    assert gm["grants_missing"] == {"weak": ["get_html"]}
    assert "update_agent" in gm["hint"]

    self_embed = await embed.embed(SELF, {"id": SELF}, k)
    assert "error" in self_embed

    missing = await embed.embed(SELF, {}, k)
    assert "error" in missing and missing["reason"] == "bad_arg"

    assert len(k.rows_of(SELF)) == before


async def test_unembed_removes_card_live_and_deleted_and_leaves_agent():
    k = PanelCtx({"panel": GOOD})
    await embed.embed(SELF, {"id": "panel"}, k)

    out = await embed.unembed(SELF, {"id": "panel"}, k)
    assert out == {"unembedded": "panel"}

    h = await _history(k)
    assert _cards(h["rows"]) == []
    tombs = [r for r in _card_rows(k) if r.get("kind") == "unembed"]
    assert len(tombs) == 1 and tombs[0]["content"] == "panel"
    assert k.sent == []

    # embed again re-shows it
    await embed.embed(SELF, {"id": "panel"}, k)
    assert _cards((await _history(k))["rows"]) == ["panel"]

    # an id never embedded: no row added
    before = len(k.rows_of(SELF))
    never = await embed.unembed(SELF, {"id": "stranger"}, k)
    assert never == {"unembedded": "stranger", "was_embedded": False}
    assert len(k.rows_of(SELF)) == before

    # a deleted agent (record None) still takes its card out
    await embed.embed(SELF, {"id": "panel"}, k)
    k.records.pop("panel")
    gone = await embed.unembed(SELF, {"id": "panel"}, k)
    assert gone == {"unembedded": "panel"}
    assert _cards((await _history(k))["rows"]) == []
    assert k.sent == []


async def test_embed_rows_never_reach_the_prompt():
    k = PanelCtx({"panel": GOOD})
    await core._save_history(SELF, k, [{"role": "user", "content": "hello there"}])

    hist_before = await core._load_history(SELF, k)
    msgs_before, _ = await core._assemble(SELF, "next", k, "c", system_prompt="sys")

    await embed.embed(SELF, {"id": "panel"}, k)
    await embed.unembed(SELF, {"id": "panel"}, k)
    await embed.embed(SELF, {"id": "panel"}, k)

    assert await core._load_history(SELF, k) == hist_before
    msgs_after, _ = await core._assemble(SELF, "next", k, "c", system_prompt="sys")
    assert msgs_after == msgs_before


def test_embed_and_unembed_docs_carry_the_chat_window_text():
    doc = core._embed.__doc__ or ""
    for needle in (
        "FOR THE CHAT WINDOW",
        "1200",
        "not_found",
        "no_view",
        "grants_missing",
    ):
        assert needle in doc, needle
    assert "tombstone" in (core._unembed.__doc__ or "")

    verbs, _ = core.build(sentence="s", make_provider=lambda *a: None, name="fake")
    keys = list(verbs)
    i = keys.index("history")
    assert keys[i + 1 : i + 3] == ["embed", "unembed"]


def test_the_anthropic_readme_has_the_chat_cards_section():
    text = ANTHROPIC_README.read_text(encoding="utf-8")
    assert "## Chat cards" in text
    assert "`embed {id}`" in text
