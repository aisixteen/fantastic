"""Agent module — @autorun decorator and AST discovery."""

import ast
from pathlib import Path
from typing import Any


def autorun(fn=None, *, pty=False, env=None):
    """Decorator marking a function as agent's autorun entry point.

    @autorun(pty=True)
    def main():
        os.execlp(shell, shell, "-l")
    """
    config = {"pty": pty, "env": env or {}}
    if fn is not None:
        fn._autorun_config = config
        return fn

    def wrapper(f):
        f._autorun_config = config
        return f

    return wrapper


def discover_autorun(source_path: Path) -> dict[str, Any] | None:
    """AST-scan source.py for @autorun decorator. Returns config or None.

    Handles:
      - @autorun / @agent.autorun (bare — no args)
      - @autorun(pty=True, env={...}) / @agent.autorun(pty=True)
      - def autorun(): ... (bare function named autorun, no decorator)
    """
    if not source_path.exists():
        return None
    source = source_path.read_text(encoding="utf-8")
    if not source.strip():
        return None

    try:
        tree = ast.parse(source, filename=str(source_path))
    except SyntaxError:
        return None

    for node in ast.walk(tree):
        if not isinstance(node, ast.FunctionDef):
            continue

        # Check decorators for @autorun or @agent.autorun
        for dec in node.decorator_list:
            if _is_autorun_decorator(dec):
                return _extract_config(dec)

        # Bare function named 'autorun' without decorator
        if node.name == "autorun" and not node.decorator_list:
            return {"pty": False, "env": {}}

    return None


def _is_autorun_decorator(dec: ast.expr) -> bool:
    """Check if a decorator is @autorun or @agent.autorun (with or without call)."""
    if isinstance(dec, ast.Name) and dec.id == "autorun":
        return True
    if isinstance(dec, ast.Call):
        return _is_autorun_decorator(dec.func)
    if isinstance(dec, ast.Attribute) and dec.attr == "autorun":
        if isinstance(dec.value, ast.Name) and dec.value.id == "agent":
            return True
    return False


def _extract_config(dec: ast.expr) -> dict[str, Any]:
    """Extract keyword args from @autorun(...) call."""
    config: dict[str, Any] = {"pty": False, "env": {}}
    if isinstance(dec, ast.Call):
        for kw in dec.keywords:
            if kw.arg == "pty" and isinstance(kw.value, ast.Constant):
                config["pty"] = bool(kw.value.value)
            elif kw.arg == "env" and isinstance(kw.value, ast.Dict):
                env = {}
                for k, v in zip(kw.value.keys, kw.value.values):
                    if isinstance(k, ast.Constant) and isinstance(v, ast.Constant):
                        env[str(k.value)] = str(v.value)
                config["env"] = env
    return config
