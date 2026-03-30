"""Plugin install helpers — clone from git or copy from local folder.

Plugins are installed into .fantastic/plugins/{name}/.
"""

import json
import logging
import shutil
import subprocess
import time
from pathlib import Path

logger = logging.getLogger(__name__)


def _is_git_url(source: str) -> bool:
    """Check if source looks like a git URL."""
    return (
        source.startswith("git@")
        or source.startswith("https://")
        or source.startswith("http://")
        or source.startswith("ssh://")
        or source.endswith(".git")
    )


def _plugins_dir(project_dir: Path) -> Path:
    return project_dir / ".fantastic" / "plugins"


def install_plugin(project_dir: Path, source: str, name: str) -> Path:
    """Install a plugin from git URL or local path into .fantastic/plugins/{name}/.

    Returns the plugin directory. Raises RuntimeError on failure.
    """
    dest = _plugins_dir(project_dir) / name
    if dest.exists():
        shutil.rmtree(dest)

    if _is_git_url(source):
        _clone_git(source, dest)
    else:
        _copy_local(Path(source), dest)

    # Validate
    if not (dest / "template.json").exists():
        shutil.rmtree(dest, ignore_errors=True)
        raise RuntimeError(f"Plugin has no template.json: {source}")

    # Record in plugins.json
    _update_plugins_json(project_dir, name, source)
    return dest


def _clone_git(url: str, dest: Path) -> None:
    """Clone a git repo to dest."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    result = subprocess.run(
        ["git", "clone", "--depth", "1", url, str(dest)],
        capture_output=True,
        text=True,
        timeout=60,
    )
    if result.returncode != 0:
        raise RuntimeError(f"git clone failed: {result.stderr.strip()}")


def _copy_local(source: Path, dest: Path) -> None:
    """Copy a local plugin directory to dest."""
    source = source.resolve()
    if not source.is_dir():
        raise RuntimeError(f"Plugin source is not a directory: {source}")
    if not (source / "template.json").exists():
        raise RuntimeError(f"Plugin has no template.json: {source}")
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source, dest)


def _plugins_json_path(project_dir: Path) -> Path:
    return project_dir / ".fantastic" / "plugins.json"


def read_plugins_json(project_dir: Path) -> dict:
    """Read .fantastic/plugins.json."""
    path = _plugins_json_path(project_dir)
    if path.exists():
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            pass
    return {}


def write_plugins_json(project_dir: Path, data: dict) -> None:
    """Write .fantastic/plugins.json."""
    path = _plugins_json_path(project_dir)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2), encoding="utf-8")


def _update_plugins_json(project_dir: Path, name: str, source: str) -> None:
    """Add/update a plugin entry in plugins.json."""
    data = read_plugins_json(project_dir)
    data[name] = {"from": source, "installed": time.time()}
    write_plugins_json(project_dir, data)


def uninstall_plugin(project_dir: Path, name: str) -> bool:
    """Remove an installed plugin from .fantastic/plugins/{name}/.

    Returns True if removed, False if not found.
    """
    dest = _plugins_dir(project_dir) / name
    if not dest.exists():
        return False
    shutil.rmtree(dest)
    # Remove from plugins.json
    data = read_plugins_json(project_dir)
    data.pop(name, None)
    write_plugins_json(project_dir, data)
    return True


def get_plugin_dir(project_dir: Path, name: str) -> Path:
    """Return .fantastic/plugins/{name}/ path."""
    return _plugins_dir(project_dir) / name


def list_plugin_dirs(project_dir: Path) -> list[Path]:
    """Return all .fantastic/plugins/*/ dirs that have template.json."""
    pdir = _plugins_dir(project_dir)
    if not pdir.exists():
        return []
    return sorted(
        d for d in pdir.iterdir() if d.is_dir() and (d / "template.json").exists()
    )
