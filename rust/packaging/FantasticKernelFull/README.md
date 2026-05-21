# FantasticKernelFull — Swift Package

The **desktop / unsandboxed** tier of the Rust kernel as a Swift Package.
Wraps `Fantastic-Full.xcframework` (Mac-only).

Registers every PTY / subprocess / dynamic-loading bundle:
`terminal_backend`, `local_runner`, `python_runtime`, `ssh_runner`.
The PTY-using bundles call `posix_spawn` / `pty_open` — APIs the iOS
sandbox forbids, which is why this XCFramework has no iOS slices.
Linking it into a sandboxed target is a build-time error by design.

Use this from unsandboxed Mac Pro builds. For sandboxed iOS / Lite use
the sister package [`FantasticKernelEmbedded`](../FantasticKernelEmbedded/).

## Build the XCFramework

```bash
cd ../..             # back to rust/
./scripts/build-xcframework-full.sh
```

Produces:
- `Fantastic-Full.xcframework/` — macos-arm64_x86_64 only
- `Sources/FantasticKernelFull/fantastic.swift` — auto-generated UniFFI bindings

The convenience wrapper `./scripts/build-xcframework.sh` builds both
embedded + full variants in one go.

## Consume from Swift

```swift
import FantasticKernelFull

let kernel = try await startKernel(
    workdir: projectURL.path,
    portHint: 0
)
let port = kernel.httpPort()
// Point WKWebView at http://127.0.0.1:\(port)/<canvas_id>/
defer { kernel.shutdown() }
```

API shape is identical to the embedded package (both wrap the same UDL
surface); only the linked binary differs.

## What the extra bundles get you (vs embedded)

| Bundle              | What it does                                                 |
|---------------------|--------------------------------------------------------------|
| `terminal_backend`  | Real PTY (zsh / bash / fish / etc.) inside the canvas.       |
|                     | Login-shell spawn (`shell -l`) so user profile loads.        |
| `local_runner`      | Spawn arbitrary user-space processes (compilers, scripts).   |
| `python_runtime`    | Inline Python eval via `uv tool run` / system Python.        |
| `ssh_runner`        | OpenSSH client subprocess for remote runs (Pro-only since    |
|                     | iOS gets a separate in-app NIOSSH client).                   |

All four operate with the user's full machine privileges — Pro Mac is
unsandboxed (`com.apple.security.app-sandbox = false`), so the spawned
processes inherit `$HOME`, `$PATH`, dotfiles, the works. Same semantics
as opening Terminal.app.

## License

AGPL-3.0-or-later (matches the parent crate).
