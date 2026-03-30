"""Tests for instance management: launch, stop, list, register, restart, SSH, broadcasts."""

import json


from core.tools._instance_tracking import (
    _launched_processes,
    _load_tracked,
    _save_tracked,
    _instance_id,
)
from core.tools._instances import (
    _launch_instance,
    _stop_instance,
    _register_instance,
    launch_instance,
    stop_instance,
    list_instances,
    restart_instance,
)
from core.tools._process_handlers import _get_state


def _cleanup_tracked():
    """Clear tracked instances file and in-memory process cache."""
    _launched_processes.clear()
    _save_tracked([])


def test_instance_id_deterministic():
    """Same path always produces same ID."""
    assert _instance_id("/tmp/proj") == _instance_id("/tmp/proj")


def test_instance_id_different_paths():
    """Different paths produce different IDs."""
    assert _instance_id("/tmp/a") != _instance_id("/tmp/b")


def test_instance_id_ssh():
    """SSH host is included in the ID."""
    local_id = _instance_id("/home/user/proj")
    ssh_id = _instance_id("/home/user/proj", ssh_host="gpu-box")
    assert local_id != ssh_id


async def test_list_instances_empty(setup):
    _cleanup_tracked()
    result = await list_instances()
    assert result == []


async def test_launch_instance(setup, tmp_path):
    """Mock asyncio.create_subprocess_exec, verify process spawned with correct args."""
    import sys
    from unittest.mock import AsyncMock, patch, MagicMock

    _cleanup_tracked()
    project = tmp_path / "child_project"
    project.mkdir()

    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.pid = 12345
    mock_proc.terminate = MagicMock()
    mock_proc.kill = MagicMock()
    mock_proc.wait = AsyncMock()

    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_client = AsyncMock()
    mock_client.get = AsyncMock(return_value=mock_response)
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)

    with (
        patch(
            "asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)
        ) as mock_exec,
        patch("httpx.AsyncClient", return_value=mock_client),
        patch(
            "core.instance_backend.LocalBackend.find_free_local_port",
            return_value=49999,
        ),
    ):
        result = await launch_instance(project_dir=str(project))

    assert "error" not in result
    assert result["port"] == 49999
    assert result["pid"] == 12345
    assert result["id"].startswith("inst_")
    assert result["project_dir"] == str(project)
    assert result["url"] == "http://127.0.0.1:49999"

    # Verify start_new_session=True
    call_kwargs = mock_exec.call_args
    assert call_kwargs.kwargs.get("start_new_session") is True

    # Verify cmd includes correct args
    cmd_args = call_kwargs.args
    assert sys.executable in cmd_args
    assert "--port" in cmd_args
    assert "49999" in cmd_args
    assert "--project-dir" in cmd_args
    assert str(project) in cmd_args

    # Verify tracked to disk
    tracked = _load_tracked()
    assert any(e["project_dir"] == str(project) for e in tracked)

    # Verify deterministic ID
    expected_id = _instance_id(str(project))
    assert result["id"] == expected_id

    _cleanup_tracked()


async def test_launch_instance_with_cli(setup, tmp_path):
    """Verify --cli args are passed through."""
    from unittest.mock import AsyncMock, patch, MagicMock

    _cleanup_tracked()
    project = tmp_path / "cli_project"
    project.mkdir()

    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.pid = 99999
    mock_proc.terminate = MagicMock()
    mock_proc.kill = MagicMock()
    mock_proc.wait = AsyncMock()

    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_client = AsyncMock()
    mock_client.get = AsyncMock(return_value=mock_response)
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)

    with (
        patch(
            "asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)
        ) as mock_exec,
        patch("httpx.AsyncClient", return_value=mock_client),
        patch(
            "core.instance_backend.LocalBackend.find_free_local_port",
            return_value=50000,
        ),
    ):
        result = await launch_instance(
            project_dir=str(project), cli="claude --model sonnet"
        )

    assert "error" not in result
    cmd_args = mock_exec.call_args.args
    assert "--cli" in cmd_args
    assert "claude" in cmd_args
    assert "--model" in cmd_args
    assert "sonnet" in cmd_args

    _cleanup_tracked()


async def test_launch_instance_bad_dir(setup):
    result = await launch_instance(project_dir="/nonexistent/path/xyz")
    assert "error" in result
    assert "does not exist" in result["error"]


async def test_stop_instance(setup, tmp_path):
    """Track + cache a mock process, verify stop terminates it."""
    from unittest.mock import AsyncMock, MagicMock

    _cleanup_tracked()
    project = tmp_path / "stop_proj"
    project.mkdir()

    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.pid = 54321
    mock_proc.terminate = MagicMock()
    mock_proc.kill = MagicMock()
    mock_proc.wait = AsyncMock(return_value=0)

    inst_id = _instance_id(str(project))
    # Add to tracked list + cache process
    _save_tracked(
        [{"project_dir": str(project), "name": "stop_proj", "backend": "local"}]
    )
    _launched_processes[inst_id] = mock_proc

    result = await stop_instance(inst_id)
    assert result["stopped"] is True
    assert result["id"] == inst_id
    mock_proc.terminate.assert_called_once()
    # Process removed from cache
    assert inst_id not in _launched_processes

    _cleanup_tracked()


async def test_stop_instance_not_found(setup):
    _cleanup_tracked()
    result = await stop_instance("inst_nonexistent")
    assert "error" in result
    assert "not found" in result["error"]


async def test_list_instances_with_running(setup, tmp_path):
    """Track an instance with a live PID but no HTTP server — shows as unresponsive."""
    import os

    _cleanup_tracked()
    project = tmp_path / "list_proj"
    project.mkdir()

    # Write config.json with our own PID (alive but no HTTP server on this port)
    config_dir = project / ".fantastic"
    config_dir.mkdir(parents=True)
    (config_dir / "config.json").write_text(
        json.dumps(
            {
                "pid": os.getpid(),
                "port": 49300,
            }
        )
    )

    _save_tracked(
        [{"project_dir": str(project), "name": "list_proj", "backend": "local"}]
    )

    result = await list_instances()
    assert len(result) == 1
    inst_id = _instance_id(str(project))
    assert result[0]["id"] == inst_id
    assert result[0]["status"] == "unresponsive"
    assert result[0]["port"] == 49300

    _cleanup_tracked()


async def test_list_instances_with_running_http(setup, tmp_path):
    """Track an instance with a live PID + responsive HTTP — shows as running."""
    import os
    from unittest.mock import AsyncMock, MagicMock, patch

    _cleanup_tracked()
    project = tmp_path / "list_proj_http"
    project.mkdir()

    config_dir = project / ".fantastic"
    config_dir.mkdir(parents=True)
    (config_dir / "config.json").write_text(
        json.dumps(
            {
                "pid": os.getpid(),
                "port": 49300,
            }
        )
    )

    _save_tracked(
        [{"project_dir": str(project), "name": "list_proj_http", "backend": "local"}]
    )

    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_client = AsyncMock()
    mock_client.get = AsyncMock(return_value=mock_response)
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)

    with patch("httpx.AsyncClient", return_value=mock_client):
        result = await list_instances()

    assert len(result) == 1
    assert result[0]["status"] == "running"

    _cleanup_tracked()


async def test_list_instances_stopped(setup, tmp_path):
    """Instance with dead PID shows as stopped."""
    _cleanup_tracked()
    project = tmp_path / "dead_proj"
    project.mkdir()

    config_dir = project / ".fantastic"
    config_dir.mkdir(parents=True)
    (config_dir / "config.json").write_text(
        json.dumps(
            {
                "pid": 999999,  # Very unlikely to be alive
                "port": 49301,
            }
        )
    )

    _save_tracked(
        [{"project_dir": str(project), "name": "dead_proj", "backend": "local"}]
    )

    result = await list_instances()
    assert len(result) == 1
    assert result[0]["status"] == "stopped"

    _cleanup_tracked()


async def test_restart_instance_not_tracked(setup):
    """restart_instance fails when ID is not tracked."""
    _cleanup_tracked()
    result = await restart_instance("inst_nonexistent")
    assert "error" in result
    assert "not found" in result["error"]


async def test_restart_instance_success(setup, tmp_path):
    """restart_instance stops old process and launches new one."""
    from unittest.mock import AsyncMock, patch, MagicMock

    _cleanup_tracked()
    project = tmp_path / "restart_project"
    project.mkdir()

    inst_id = _instance_id(str(project))

    # Track the instance and cache a mock old process
    _save_tracked(
        [{"project_dir": str(project), "name": "to-restart", "backend": "local"}]
    )
    mock_old_proc = AsyncMock()
    mock_old_proc.returncode = None
    mock_old_proc.pid = 11111
    mock_old_proc.terminate = MagicMock()
    mock_old_proc.kill = MagicMock()
    mock_old_proc.wait = AsyncMock(return_value=0)
    _launched_processes[inst_id] = mock_old_proc

    # Mock new launch
    mock_new_proc = AsyncMock()
    mock_new_proc.returncode = None
    mock_new_proc.pid = 22222
    mock_new_proc.terminate = MagicMock()
    mock_new_proc.kill = MagicMock()
    mock_new_proc.wait = AsyncMock()

    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_client = AsyncMock()
    mock_client.get = AsyncMock(return_value=mock_response)
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)

    with (
        patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_new_proc)),
        patch("httpx.AsyncClient", return_value=mock_client),
        patch(
            "core.instance_backend.LocalBackend.find_free_local_port",
            return_value=49300,
        ),
    ):
        result = await restart_instance(inst_id)

    assert "error" not in result
    assert result["restarted_from"] == inst_id
    assert result["id"] == inst_id  # Deterministic: same path = same ID
    assert result["port"] == 49300
    assert result["pid"] == 22222

    # Old process should have been terminated
    mock_old_proc.terminate.assert_called_once()

    # New process cached under same ID
    assert inst_id in _launched_processes

    _cleanup_tracked()


# ─── SSH launch ──────────────────────────────────────────────────────────


async def test_launch_instance_ssh(setup, tmp_path):
    """launch_instance with ssh_host delegates to SSHBackend."""
    from unittest.mock import AsyncMock, patch, MagicMock

    _cleanup_tracked()

    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.pid = 77777
    mock_proc.terminate = MagicMock()
    mock_proc.kill = MagicMock()
    mock_proc.wait = AsyncMock()

    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_client = AsyncMock()
    mock_client.get = AsyncMock(return_value=mock_response)
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)

    with (
        patch(
            "core.instance_backend.SSHBackend._find_free_remote_port",
            AsyncMock(return_value=9123),
        ),
        patch(
            "core.instance_backend.SSHBackend._check_remote_server",
            AsyncMock(return_value=None),
        ),
        patch("asyncio.create_subprocess_shell", AsyncMock(return_value=mock_proc)),
        patch("httpx.AsyncClient", return_value=mock_client),
        patch(
            "core.instance_backend.SSHBackend.find_free_local_port", return_value=49500
        ),
    ):
        result = await launch_instance(
            project_dir="/home/user/proj",
            ssh_host="gpu-box",
        )

    assert "error" not in result
    assert result["backend"] == "ssh"
    assert result["ssh_host"] == "gpu-box"
    assert result["port"] == 49500
    assert result["pid"] == 77777

    # Verify tracked entry includes SSH metadata
    tracked = _load_tracked()
    ssh_entries = [e for e in tracked if e.get("ssh_host") == "gpu-box"]
    assert len(ssh_entries) == 1
    assert ssh_entries[0]["tunnel_pid"] == 77777

    _cleanup_tracked()


# ─── Instance broadcasts ──────────────────────────────────────────────


async def test_launch_instance_broadcasts(setup, tmp_path):
    """_launch_instance ToolResult has instances_changed broadcast."""
    from unittest.mock import AsyncMock, patch, MagicMock

    _cleanup_tracked()
    project = tmp_path / "bc_proj"
    project.mkdir()

    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.pid = 40001
    mock_proc.terminate = MagicMock()
    mock_proc.kill = MagicMock()
    mock_proc.wait = AsyncMock()

    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_client = AsyncMock()
    mock_client.get = AsyncMock(return_value=mock_response)
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)

    with (
        patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)),
        patch("httpx.AsyncClient", return_value=mock_client),
        patch(
            "core.instance_backend.LocalBackend.find_free_local_port",
            return_value=49401,
        ),
    ):
        tr = await _launch_instance(str(project))

    assert any(b["type"] == "instances_changed" for b in tr.broadcast)
    bc_msg = [b for b in tr.broadcast if b["type"] == "instances_changed"][0]
    assert "instances" in bc_msg

    _cleanup_tracked()


async def test_stop_instance_broadcasts(setup, tmp_path):
    """_stop_instance ToolResult has instances_changed broadcast."""
    from unittest.mock import AsyncMock, MagicMock

    _cleanup_tracked()
    project = tmp_path / "bc_stop_proj"
    project.mkdir()

    inst_id = _instance_id(str(project))
    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.pid = 50001
    mock_proc.terminate = MagicMock()
    mock_proc.kill = MagicMock()
    mock_proc.wait = AsyncMock(return_value=0)

    _save_tracked(
        [{"project_dir": str(project), "name": "bc_stop", "backend": "local"}]
    )
    _launched_processes[inst_id] = mock_proc

    tr = await _stop_instance(inst_id)
    assert any(b["type"] == "instances_changed" for b in tr.broadcast)

    _cleanup_tracked()


# ─── get_state + instances ──────────────────────────────────────────────


async def test_get_state_includes_instances(setup, tmp_path):
    """_get_state() reply includes instances key when tracked instances exist."""
    import os

    _cleanup_tracked()
    project = tmp_path / "gs_proj"
    project.mkdir()

    config_dir = project / ".fantastic"
    config_dir.mkdir(parents=True)
    (config_dir / "config.json").write_text(
        json.dumps(
            {
                "pid": os.getpid(),
                "port": 49206,
            }
        )
    )

    _save_tracked(
        [{"project_dir": str(project), "name": "gs_proj", "backend": "local"}]
    )

    tr = await _get_state()
    assert "instances" in tr.data
    assert len(tr.data["instances"]) >= 1

    _cleanup_tracked()


async def test_get_state_no_instances_key_when_empty(setup):
    """_get_state() reply omits instances when none exist."""
    _cleanup_tracked()
    tr = await _get_state()
    assert "instances" not in tr.data


# ─── register / unregister / self-protection ─────────────────────────


async def test_register_instance_rejects_self(setup):
    """register_instance should reject registering this server's own URL."""
    from unittest.mock import patch

    with patch("core.tools._instance_tracking._get_own_port", return_value=8888):
        tr = await _register_instance(
            url="http://127.0.0.1:8888", project_dir="/tmp/self"
        )

    assert "error" in tr.data
    assert "Cannot register self" in tr.data["error"]


async def test_register_instance_requires_project_dir(setup):
    """register_instance requires project_dir."""
    tr = await _register_instance(url="http://127.0.0.1:49200")
    assert "error" in tr.data
    assert "project_dir is required" in tr.data["error"]


async def test_register_instance_adds_to_tracked(setup, tmp_path):
    """register_instance adds an entry to the tracked list."""
    _cleanup_tracked()
    project = tmp_path / "reg_proj"
    project.mkdir()

    tr = await _register_instance(
        url="http://127.0.0.1:49200",
        project_dir=str(project),
        name="registered-peer",
    )

    assert "error" not in tr.data
    assert tr.data["project_dir"] == str(project)
    assert tr.data["name"] == "registered-peer"

    # Verify it's in the tracked list
    tracked = _load_tracked()
    assert any(e["project_dir"] == str(project) for e in tracked)

    _cleanup_tracked()


async def test_launch_rejects_self_pid(setup, tmp_path):
    """launch_instance should reject if launched process has same PID as us."""
    import os
    from unittest.mock import AsyncMock, patch, MagicMock

    _cleanup_tracked()
    project = tmp_path / "self_launch"
    project.mkdir()

    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.pid = os.getpid()  # Same PID as current process
    mock_proc.terminate = MagicMock()
    mock_proc.kill = MagicMock()
    mock_proc.wait = AsyncMock()

    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_client = AsyncMock()
    mock_client.get = AsyncMock(return_value=mock_response)
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)

    with (
        patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)),
        patch("httpx.AsyncClient", return_value=mock_client),
        patch(
            "core.instance_backend.LocalBackend.find_free_local_port",
            return_value=49777,
        ),
    ):
        tr = await _launch_instance(project_dir=str(project))

    assert "error" in tr.data
    assert "Cannot track self" in tr.data["error"]

    _cleanup_tracked()


async def test_stop_does_not_remove_from_tracked(setup, tmp_path):
    """stop_instance does NOT remove the entry from tracked list."""
    from unittest.mock import AsyncMock, MagicMock

    _cleanup_tracked()
    project = tmp_path / "keep_tracked_proj"
    project.mkdir()

    inst_id = _instance_id(str(project))
    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.pid = 54321
    mock_proc.terminate = MagicMock()
    mock_proc.kill = MagicMock()
    mock_proc.wait = AsyncMock(return_value=0)

    _save_tracked([{"project_dir": str(project), "name": "keep", "backend": "local"}])
    _launched_processes[inst_id] = mock_proc

    result = await stop_instance(inst_id)
    assert result["stopped"] is True

    # Entry should still be in tracked list
    tracked = _load_tracked()
    assert any(e["project_dir"] == str(project) for e in tracked)

    _cleanup_tracked()
