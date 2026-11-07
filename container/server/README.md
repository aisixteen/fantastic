# The server container

The same `fantastic` CLI, headless, in a container: one canvas on a port, for a
server or a CI box. It runs under Docker and under rootless Podman, on amd64 and
arm64, from `ghcr.io/aisixteen/fantastic` (tags `X.Y.Z`, the commit sha, and
`latest` for the newest release). Its base is Debian 13 "trixie";
the `fantastic` binary is the one the `.deb` carries. The recipe is
`container/server/Containerfile`; the entrypoint is `container/server/fantastic-server`.

> **The canvas has terminals.** Anyone who reaches the port runs commands as the
> container's user. Publish it on `127.0.0.1` only, never on a public interface,
> and reach a remote server through an SSH tunnel.

```sh
# Docker: the canvas on this machine's loopback, its state and its folder kept
docker run -d --name fantastic -p 127.0.0.1:8088:8088 \
  -v fantastic-state:/home/fantastic/.fantastic -v fantastic-work:/work \
  ghcr.io/aisixteen/fantastic:latest

# Podman, rootless: the same
podman run -d --name fantastic -p 127.0.0.1:8088:8088 \
  -v fantastic-state:/home/fantastic/.fantastic -v fantastic-work:/work \
  ghcr.io/aisixteen/fantastic:latest

# your own uid, with your own folders for the state and the canvas: the container runs
# as you, so both must be host folders you own (a named volume belongs to uid 1000)
docker run … --user "$(id -u):$(id -g)" -v "$PWD/state:/home/fantastic/.fantastic" -v "$PWD/canvas:/work" …   # Docker
podman run … --userns=keep-id -v "$PWD/state:/home/fantastic/.fantastic" -v "$PWD/canvas:/work" …            # Podman
```

- **Ports.** The server listens on `8088` inside the container
  (`FANTASTIC_PORT`), and the Host guard admits the loopback names on that port: map
  it as `-p 127.0.0.1:<port>:<port>` (the same port on both sides). Any other mapping
  reaches the server, but the browser's address is refused with a 403. For another
  port, set both: `-e FANTASTIC_PORT=9000 -p 127.0.0.1:9000:9000`.
- **One canvas.** One container is one canvas, with no headless home beside it. To
  serve another canvas, run another container with its own volumes and port.
- **No login.** The port's reach is the access: the Host guard checks the address,
  not a password. Keep the port on loopback, or behind an SSH tunnel.
- **Volumes.** `/home/fantastic/.fantastic` is the state (the saved set-ups and the
  shared stores); `/work` is the canvas folder. A new container on the same
  volumes keeps everything: `docker stop` then `docker start`, or a new `run`.
- **Your uid.** Without `--user` or `--userns=keep-id` the container runs as uid 1000,
  which owns `/home/fantastic` and its `.fantastic`. With them it runs as your uid, so
  the state must be a host folder you own as well: with the named state volume the
  container reports healthy, but the saved set-ups and the shared stores fail later
  with Permission denied. The preflight checks `/work` only.
- **Stop.** `docker stop` / `podman stop` send SIGTERM. The kernel drains and exits
  0, so the stop is orderly; tini forwards the signal and reaps the children.
- **Recipes.** Arguments after the image are recipes, as on the command line:
  `… ghcr.io/aisixteen/fantastic:latest ai ollama -p 11434`.
- **Health.** The image's `HEALTHCHECK` asks the canvas page for a 200, on the
  `localhost` Host a browser sends. The build uses Docker's image format, so Podman
  keeps it too: `docker inspect` shows its status, and `podman healthcheck run
  fantastic` runs it once.
- **Remote, through a tunnel.** On the server, the same `run` as above. On your
  machine, the local port equal to the container's:

  ```sh
  ssh -N -L 8088:127.0.0.1:8088 you@server
  ```

  then open `http://localhost:8088/canvas_page/file/index.html`.
- **Opening it.** Open `http://localhost:8088/canvas_page/file/index.html` (or the same path on `127.0.0.1`). The
  container binds `0.0.0.0`, so the port map reaches it; the Host guard admits the
  loopback names on that port. The server prints this address at start (`docker
  logs fantastic`). The root `/` is not a page, so a bare `http://localhost:8088/`
  is a 404. Any other name, a LAN address or another port is refused with a 403, and
  so is a DNS-rebinding page. A browser cannot open `0.0.0.0`, so do not use it as
  the address.

The image's build is `scripts/build/container.sh` (`--arch amd64|arm64`, one
image, loaded locally; `--arch all --push`, the manifest). CI builds both
architectures and smoke-tests them under Docker and rootless Podman
(`container/server/container_check.py`: starts, answers, a canvas page loads, a terminal runs
a command, stops with SIGTERM, restarts on the same volumes with its state kept).
The image is pushed only by `publish-container.yml`, by hand, from a green full
run. That workflow rebuilds the image from the run's archives rather than pushing
the smoke-tested image: the binary and the pinned base are the same, but the apt
packages are not pinned, so the pushed bytes can differ from the tested ones.

## Security

The server container listens on every address inside its network namespace, and its
port reaches the host only through the `-p 127.0.0.1:` map (see above). The shipped
image has no debugging surface: no WebKit inspector, no WebDriver and no forwarder.

## CI

`ci.yml` job `container` (amd64 and arm64 on the native free runners; Docker and
rootless Podman): `container.sh` builds the image for the arch, and `container_check.py`
runs the smoke test under each engine (the container starts and reports healthy, the
kernel answers, a canvas page loads, a terminal runs a command, SIGTERM exits 0, and
a restart on the same volumes keeps the state). It keeps `container-check-<arch>`
(`container_check-<engine>-<arch>.log` and each container's log) for 14 days. The
check's socket helpers are `kernel_client.py`; its unit tests are `test_container_check.py`
(`python3 -m unittest discover -s container/server`).
