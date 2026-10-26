"""Role cards: one file = a `---` YAML header (`name`, `purpose`, `tier`, `grants`)
and the directive text (the system prompt) after it. Mirror of the rust `cards.rs`."""

from __future__ import annotations

from pathlib import Path

import yaml

from kernel._agent import Agent

#: The generic card that ships in the package, listed even when no folder has a copy.
PROJECT_LEAD = (Path(__file__).parent / "cards" / "project_lead.md").read_text(
    encoding="utf-8"
)

#: A card's grant key that stands for the registry's own id.
REGISTRY_TOKEN = "$registry"


def _field(h: dict, key: str, file: str) -> str:
    v = h.get(key)
    if isinstance(v, str) and v.strip():
        return v.strip()
    raise ValueError(f"card {file}: {key} required")


def parse_card(text: str, file: str) -> dict:
    """Parse one card into `{name, purpose, tier, grants, directive}`. Raises
    `ValueError` with the reason as one line: `card <file>: missing header`,
    `card <file>: <field> required`, `card <file>: grants must be a map of "*" or
    verb lists`."""
    missing = f"card {file}: missing header"
    if text.startswith("---\n"):
        rest = text[4:]
    elif text.startswith("---\r\n"):
        rest = text[5:]
    else:
        raise ValueError(missing)
    i = rest.find("\n---")
    if i < 0:
        raise ValueError(missing)
    after = rest[i + 4 :]
    if after.startswith("\r\n"):
        body = after[2:]
    elif after.startswith("\n"):
        body = after[1:]
    elif after == "":
        body = after
    else:
        raise ValueError(missing)
    header = rest[:i]
    try:
        h = yaml.safe_load(header)
    except yaml.YAMLError as e:
        raise ValueError(f"card {file}: header is not YAML ({e})") from None
    if not isinstance(h, dict):
        raise ValueError(missing)
    name = _field(h, "name", file)
    purpose = _field(h, "purpose", file)
    tier = _field(h, "tier", file)
    g = h.get("grants")
    grants: dict = {}
    if g is not None:
        shape_err = f'card {file}: grants must be a map of "*" or verb lists'
        if not isinstance(g, dict):
            raise ValueError(shape_err)
        swapped = {("$self" if k == REGISTRY_TOKEN else k): v for k, v in g.items()}
        if Agent._bad_grant_shape({"can_call": swapped}) is not None:
            raise ValueError(shape_err)
        grants = dict(g)
    return {
        "name": name,
        "purpose": purpose,
        "tier": tier,
        "grants": grants,
        "directive": body.strip(),
    }


def load_dir(dirs: list[str]) -> tuple[list[dict], list[str]]:
    """Every card: the shipped `project_lead`, then each `*.md` of each folder in
    `dirs` (files in name order). A bad card is listed in the errors and skipped; a
    second card with a name already read from a folder is refused. A folder card
    replaces the shipped one of the same name."""
    cards: list[dict] = []
    errors: list[str] = []
    try:
        cards.append(parse_card(PROJECT_LEAD, "project_lead.md"))
    except ValueError as e:
        errors.append(str(e))
    from_dirs: set[str] = set()
    for d in dirs:
        try:
            files = sorted(p for p in Path(d).iterdir() if p.suffix == ".md")
        except OSError:
            errors.append(f"role dir {d}: cannot be read")
            continue
        for p in files:
            try:
                text = p.read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError) as e:
                errors.append(f"card {p.name}: unreadable ({e})")
                continue
            try:
                c = parse_card(text, p.name)
            except ValueError as e:
                errors.append(str(e))
                continue
            if c["name"] in from_dirs:
                errors.append(f"card {p.name}: duplicate role {c['name']}")
                continue
            from_dirs.add(c["name"])
            cards = [k for k in cards if k["name"] != c["name"]]
            cards.append(c)
    return cards, errors
