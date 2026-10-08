"""E64 mcp_config — `.mcp.json` at the project root, kept by the `mcp` leg
through its own writable bridge (`mcp_config`), at every boot:

a) a file with ANOTHER server already in it: after the first boot it also holds
   `mcpServers.fantastic = {command: <the binary, absolute>, args: [mcp]}`, the
   other server (and its key order) untouched;
b) a second boot leaves the file byte-identical (written only when it changed);
c) the leg reports no `mcp_config_error`; the bridge exposes only `.mcp.json`."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402

OTHER = {"z_other": {"command": "/opt/other/bin/srv", "args": ["--stdio"], "env": {"K": "v"}}}


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    cfg = rig.proj / ".mcp.json"
    cfg.write_text(json.dumps({"mcpServers": OTHER, "keep": 1}, indent=2))
    r: dict = {}
    info: dict = {}
    try:
        rig.init()
        doc = json.loads(cfg.read_text())
        info["file"] = doc
        servers = doc.get("mcpServers", {})
        fan = servers.get("fantastic", {})
        r["entry"] = fan.get("args") == ["mcp"] and Path(str(fan.get("command", ""))).is_absolute() and Path(fan["command"]).name == "fantastic"
        r["other_kept"] = servers.get("z_other") == OTHER["z_other"] and doc.get("keep") == 1 and list(servers)[0] == "z_other"
        before = cfg.read_bytes()
        rig.stop()
        rig.start()
        r["unchanged_on_reboot"] = cfg.read_bytes() == before
        p = subprocess.run([str(rig.binary), "send", json.dumps({"target": "mcp", "payload": {"type": "reflect", "tree": "none"}})],
                           cwd=rig.proj, env=rig.env(), capture_output=True, text=True, timeout=60)
        refl = (json.loads(p.stdout).get("result") or {}) if p.stdout.strip().startswith("{") else {}
        info["leg"] = {k: refl.get(k) for k in ("file_bridge_id", "mcp_config_file", "mcp_config_error")}
        r["no_error"] = bool(refl) and not refl.get("mcp_config_error")
        import yaml
        b = next((yaml.safe_load(f.read_text()) for f in (rig.proj / ".fantastic" / "agents").rglob("mcp_config/agent.yaml")), {})
        r["bridge_only_file"] = b.get("allow") == [".mcp.json"] and b.get("root") == "." and not b.get("readonly")
    finally:
        rig.stop()
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
