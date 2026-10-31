# Aisixteen Fantastic, the desktop app

The Tauri 2 shell around the home kernel. `scripts/build/app.sh` builds it; `SMOKE.md` is
the by-hand check.

## Build the Flatpak locally

The Flatpak (#82) is built by `flatpak-builder` from `packaging/flatpak/oleksandr.aisixteen.fantastic.desktop.yml`,
offline: the crates and npm packages are vendored by the `packaging/flatpak/*-sources.json`
files (regenerate them with `sh packaging/flatpak/generate-sources.sh` after a lockfile
changes). Linux only; install `flatpak` and `flatpak-builder` from your distro, then, from
the repository root:

```sh
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak-builder --user --install-deps-from=flathub --force-clean --disable-rofiles-fuse \
  build-dir packaging/flatpak/oleksandr.aisixteen.fantastic.desktop.yml
```

`--install-deps-from=flathub` fetches the GNOME runtime and the Rust, Go and Node SDK
extensions on the first run. To run the result without installing it:

```sh
flatpak-builder --run build-dir packaging/flatpak/oleksandr.aisixteen.fantastic.desktop.yml fantastic-app
```

or install it and start it:

```sh
flatpak-builder --user --install --force-clean build-dir packaging/flatpak/oleksandr.aisixteen.fantastic.desktop.yml
flatpak run oleksandr.aisixteen.fantastic.desktop
```

The sandbox in the manifest (the whole home, the network, the host-spawn portal) is a wide
PLACEHOLDER: the maintainer will revisit it. CI builds the Flatpak only on a manual run,
`gh workflow run ci.yml -f flatpak=true`; it uploads and publishes nothing.
