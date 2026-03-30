"""Tests for instance_backend — LocalBackend, SSHBackend, factory."""

import signal as _signal

import pytest
from unittest.mock import AsyncMock, MagicMock, patch

from core.instance_backend import (
    LaunchResult,
    LocalBackend,
    SSHBackend,
    get_backend,
)


# ─── Factory ──────────────────────────────────────────────────────────────


def test_get_backend_local():
    backend = get_backend()
    assert isinstance(backend, LocalBackend)
    assert backend.backend_type == "local"


def test_get_backend_local_explicit_none():
    backend = get_backend(ssh_host=None)
    assert isinstance(backend, LocalBackend)


def test_get_backend_ssh():
    backend = get_backend(ssh_host="gpu-box")
    assert isinstance(backend, SSHBackend)
    assert backend.backend_type == "ssh"
    assert backend._ssh_host == "gpu-box"


def test_get_backend_ssh_custom_cmd():
    backend = get_backend(ssh_host="gpu-box", remote_cmd="/opt/bin/fantastic")
    assert isinstance(backend, SSHBackend)
    assert backend._remote_cmd == "/opt/bin/fantastic"


# ─── LocalBackend.launch ─────────────────────────────────────────────────


async def test_local_launch_success(tmp_path):
    project = tmp_path / "proj"
    project.mkdir()

    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.pid = 12345
    mock_proc.terminate = MagicMock()

    backend = LocalBackend()

    with (
        patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)),
        patch.object(backend, "health_check", AsyncMock(return_value=True)),
        patch.object(backend, "find_free_local_port", return_value=49999),
    ):
        result = await backend.launch(str(project))

    assert isinstance(result, LaunchResult)
    assert result.pid == 12345
    assert result.port == 49999
    assert result.url == "http://127.0.0.1:49999"
    assert result.process is mock_proc


async def test_local_launch_bad_dir():
    backend = LocalBackend()
    with pytest.raises(RuntimeError, match="does not exist"):
        await backend.launch("/nonexistent/path/xyz")


async def test_local_launch_process_dies(tmp_path):
    project = tmp_path / "proj"
    project.mkdir()

    mock_proc = AsyncMock()
    mock_proc.returncode = 1
    mock_proc.pid = 12345
    mock_proc.stderr = AsyncMock()
    mock_proc.stderr.read = AsyncMock(return_value=b"startup error")
    mock_proc.terminate = MagicMock()

    backend = LocalBackend()

    with (
        patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)),
        patch.object(backend, "find_free_local_port", return_value=50000),
    ):
        with pytest.raises(RuntimeError, match="startup error"):
            await backend.launch(str(project))


# ─── LocalBackend.stop ───────────────────────────────────────────────────


async def test_local_stop_clean():
    """stop(pid) sends SIGTERM to process group; process exits quickly."""
    backend = LocalBackend()

    def fake_kill(pid, sig):
        if sig == 0:
            raise ProcessLookupError()

    with (
        patch("os.getpgid", return_value=1234),
        patch("os.killpg") as mock_killpg,
        patch("os.kill", side_effect=fake_kill),
    ):
        await backend.stop(1234)
    mock_killpg.assert_called_once_with(1234, _signal.SIGTERM)


# ─── SSHBackend._find_free_remote_port ───────────────────────────────────


async def test_ssh_find_remote_port_success():
    mock_proc = AsyncMock()
    mock_proc.returncode = 0
    mock_proc.communicate = AsyncMock(return_value=(b"9123\n", b""))

    backend = SSHBackend(ssh_host="gpu-box")

    with patch("asyncio.create_subprocess_shell", AsyncMock(return_value=mock_proc)):
        port = await backend._find_free_remote_port()
    assert port == 9123


async def test_ssh_find_remote_port_failure():
    mock_proc = AsyncMock()
    mock_proc.returncode = 255
    mock_proc.communicate = AsyncMock(return_value=(b"", b"Connection refused"))

    backend = SSHBackend(ssh_host="gpu-box")

    with patch("asyncio.create_subprocess_shell", AsyncMock(return_value=mock_proc)):
        with pytest.raises(RuntimeError, match="SSH port-finding failed"):
            await backend._find_free_remote_port()


# ─── SSHBackend.launch ───────────────────────────────────────────────────


async def test_ssh_launch_success():
    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.pid = 54321
    mock_proc.terminate = MagicMock()

    backend = SSHBackend(ssh_host="gpu-box")

    with (
        patch.object(backend, "_find_free_remote_port", AsyncMock(return_value=9123)),
        patch.object(backend, "_check_remote_server", AsyncMock(return_value=None)),
        patch(
            "asyncio.create_subprocess_shell", AsyncMock(return_value=mock_proc)
        ) as mock_exec,
        patch.object(backend, "health_check", AsyncMock(return_value=True)),
        patch.object(backend, "find_free_local_port", return_value=49200),
    ):
        result = await backend.launch("/home/user/proj")

    assert isinstance(result, LaunchResult)
    assert result.pid == 54321
    assert result.port == 49200
    assert result.url == "http://127.0.0.1:49200"
    assert result.extra["ssh_host"] == "gpu-box"
    assert result.extra["remote_port"] == 9123
    assert result.extra["local_port"] == 49200

    # Verify SSH command includes tunnel args (single shell string)
    cmd_str = mock_exec.call_args.args[0]
    assert "ssh" in cmd_str
    assert "-L" in cmd_str
    assert "49200:localhost:9123" in cmd_str
    assert "gpu-box" in cmd_str


async def test_ssh_launch_connection_failure():
    backend = SSHBackend(ssh_host="bad-host")

    with patch.object(
        backend,
        "_find_free_remote_port",
        AsyncMock(
            side_effect=RuntimeError(
                "SSH port-finding failed on bad-host: Connection refused"
            )
        ),
    ):
        with pytest.raises(RuntimeError, match="SSH port-finding failed"):
            await backend.launch("/home/user/proj")


# ─── SSHBackend.stop ─────────────────────────────────────────────────────


async def test_ssh_stop_terminates():
    """stop(pid) sends SIGTERM to tunnel process."""
    backend = SSHBackend(ssh_host="gpu-box")

    call_count = 0

    def fake_kill(pid, sig):
        nonlocal call_count
        if sig == _signal.SIGTERM:
            return  # SIGTERM sent
        if sig == 0:
            call_count += 1
            if call_count >= 1:
                raise ProcessLookupError()  # process already dead

    with patch("os.kill", side_effect=fake_kill) as mock_kill:
        await backend.stop(54321)
    # First call should be SIGTERM
    mock_kill.assert_any_call(54321, _signal.SIGTERM)


# ─── health_check ────────────────────────────────────────────────────────


async def test_health_check_success():
    backend = LocalBackend()
    mock_response = MagicMock()
    mock_response.status_code = 200

    mock_client = AsyncMock()
    mock_client.get = AsyncMock(return_value=mock_response)
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)

    with patch("httpx.AsyncClient", return_value=mock_client):
        result = await backend.health_check(
            "http://127.0.0.1:49200/api/state", timeout=2
        )
    assert result is True


async def test_health_check_timeout():
    backend = LocalBackend()

    mock_client = AsyncMock()
    mock_client.get = AsyncMock(side_effect=Exception("Connection refused"))
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)

    with patch("httpx.AsyncClient", return_value=mock_client):
        result = await backend.health_check(
            "http://127.0.0.1:49200/api/state", timeout=1
        )
    assert result is False


# ─── find_free_local_port ────────────────────────────────────────────────


def test_find_free_local_port():
    backend = LocalBackend()
    port = backend.find_free_local_port()
    assert 49200 <= port < 49300
