"""file bundle — filesystem root as an agent.

Each file agent owns a `root` directory and a `readonly` flag. Path safety:
every verb resolves `root/path` and refuses if the result escapes `root`.

Verbs:
  reflect            -> {sentence, root, readonly, hidden_count}
  list   path?       -> {files: [{name, path, type, size?}, ...]}
  read   path        -> {path, content} | {path, image_base64, mime}
  write  path text   -> {path, written: true}            (refused if readonly)
  delete path        -> {path, deleted: true}            (refused if readonly)
  rename old_path new_path -> {old_path, new_path}        (refused if readonly)
  mkdir  path        -> {path, created: true}            (refused if readonly)
"""

from __future__ import annotations

import base64
import os
from pathlib import Path

DEFAULT_HIDDEN = [".git", ".env", ".fantastic", "node_modules", "__pycache__"]
IMAGE_EXT = {".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"}


def _root(rec: dict) -> Path:
    r = rec.get("root", "") or ""
    if not r:
        return Path.cwd()
    return Path(r).expanduser().resolve()


def _hidden(rec: dict) -> list[str]:
    return list(rec.get("hidden", DEFAULT_HIDDEN))


def _is_hidden(name: str, hidden: list[str]) -> bool:
    return name in hidden or any(name == p for p in hidden)


def _resolve_safe(rec: dict, path: str) -> Path:
    root = _root(rec)
    rel = (path or "").lstrip("/")
    target = (root / rel).resolve()
    try:
        target.relative_to(root)
    except ValueError:
        raise ValueError(f"path {path!r} escapes root")
    return target


def _readonly_or_none(rec: dict) -> dict | None:
    if rec.get("readonly"):
        return {"error": "agent is readonly"}
    return None


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + filesystem root + readonly flag. No args."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "Filesystem root.",
        "root": str(_root(rec)),
        "readonly": bool(rec.get("readonly")),
        "hidden_count": len(_hidden(rec)),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, kernel):
    """No-op. Returns None."""
    return None


async def _list(id, payload, kernel):
    """args: path:str (default ''). Returns {path, files:[{name, path, type, size?}, ...]}. Hides DEFAULT_HIDDEN entries (.git, .env, .fantastic, …)."""
    rec = kernel.get(id) or {}
    path = payload.get("path", "")
    try:
        target = _resolve_safe(rec, path)
    except ValueError as e:
        return {"error": str(e)}
    if not target.exists():
        return {"error": f"path {path!r} does not exist"}
    if not target.is_dir():
        return {"error": f"path {path!r} is not a directory"}
    hidden = _hidden(rec)
    files = []
    try:
        for entry in sorted(target.iterdir()):
            if _is_hidden(entry.name, hidden):
                continue
            rel = str(entry.relative_to(_root(rec)))
            if entry.is_dir():
                files.append({"name": entry.name, "path": rel, "type": "dir"})
            else:
                try:
                    size = entry.stat().st_size
                except OSError:
                    size = -1
                files.append(
                    {
                        "name": entry.name,
                        "path": rel,
                        "type": "file",
                        "size": size,
                    }
                )
    except PermissionError as e:
        return {"error": str(e)}
    return {"path": path, "files": files}


async def _read(id, payload, kernel):
    """args: path:str (req). Returns {path, content:str} for text files, {path, image_base64, mime} for images. Refuses paths outside root."""
    rec = kernel.get(id) or {}
    path = payload.get("path", "")
    try:
        target = _resolve_safe(rec, path)
    except ValueError as e:
        return {"error": str(e)}
    if not target.exists() or not target.is_file():
        return {"error": f"file {path!r} not found"}
    ext = target.suffix.lower()
    if ext in IMAGE_EXT:
        data = target.read_bytes()
        mime = {
            ".png": "image/png",
            ".jpg": "image/jpeg",
            ".jpeg": "image/jpeg",
            ".gif": "image/gif",
            ".webp": "image/webp",
            ".svg": "image/svg+xml",
        }.get(ext, "application/octet-stream")
        return {
            "path": path,
            "image_base64": base64.b64encode(data).decode("ascii"),
            "mime": mime,
        }
    try:
        content = target.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return {"error": "binary file (use image read for supported formats)"}
    return {"path": path, "content": content}


async def _write(id, payload, kernel):
    """args: path:str (req), content:str (req). Returns {path, written:true, bytes}. Creates parent dirs. Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    path = payload.get("path", "")
    content = payload.get("content", "")
    try:
        target = _resolve_safe(rec, path)
    except ValueError as e:
        return {"error": str(e)}
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")
    return {"path": path, "written": True, "bytes": len(content.encode("utf-8"))}


async def _delete(id, payload, kernel):
    """args: path:str (req). Returns {path, deleted:true}. Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    path = payload.get("path", "")
    try:
        target = _resolve_safe(rec, path)
    except ValueError as e:
        return {"error": str(e)}
    if not target.exists():
        return {"error": f"path {path!r} does not exist"}
    try:
        if target.is_dir():
            os.rmdir(target)
        else:
            target.unlink()
    except OSError as e:
        return {"error": str(e)}
    return {"path": path, "deleted": True}


async def _rename(id, payload, kernel):
    """args: old_path:str, new_path:str (both req). Returns {old_path, new_path}. Creates new parent dirs. Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    old = payload.get("old_path", "")
    new = payload.get("new_path", "")
    try:
        o = _resolve_safe(rec, old)
        n = _resolve_safe(rec, new)
    except ValueError as e:
        return {"error": str(e)}
    if not o.exists():
        return {"error": f"path {old!r} does not exist"}
    n.parent.mkdir(parents=True, exist_ok=True)
    o.rename(n)
    return {"old_path": old, "new_path": new}


async def _mkdir(id, payload, kernel):
    """args: path:str (req). Returns {path, created:true}. Recursive (parents ok). Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    path = payload.get("path", "")
    try:
        target = _resolve_safe(rec, path)
    except ValueError as e:
        return {"error": str(e)}
    target.mkdir(parents=True, exist_ok=True)
    return {"path": path, "created": True}


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "list": _list,
    "read": _read,
    "write": _write,
    "delete": _delete,
    "rename": _rename,
    "mkdir": _mkdir,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"file: unknown type {t!r}"}
    return await fn(id, payload, kernel)
