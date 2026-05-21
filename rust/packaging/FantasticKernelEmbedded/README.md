# FantasticKernelEmbedded — Swift Package

The **sandboxed / iOS-safe** tier of the Rust kernel as a Swift Package.
Wraps `Fantastic-Embedded.xcframework`.

Compile-time excludes every PTY / subprocess / dynamic-loading bundle:
`terminal_backend`, `local_runner`, `python_runtime`, `ssh_runner`.
What's left is App-Sandbox safe and App-Store-compliant.

Use this from iOS / iPadOS / sandboxed-macOS Lite builds. For
unsandboxed Pro Mac use the sister package
[`FantasticKernelFull`](../FantasticKernelFull/).

## Build the XCFramework

```bash
cd ../..             # back to rust/
./scripts/build-xcframework-embedded.sh
```

Produces:
- `Fantastic-Embedded.xcframework/` — ios-arm64 + ios-arm64-simulator + macos-arm64_x86_64
- `Sources/FantasticKernelEmbedded/fantastic.swift` — auto-generated UniFFI bindings

The convenience wrapper `./scripts/build-xcframework.sh` builds both
embedded + full variants in one go.

## Consume from Swift

```swift
import FantasticKernelEmbedded

let kernel = try await startKernel(
    workdir: appGroupURL.path,
    portHint: 0
)
let port = kernel.httpPort()
// Point WKWebView at http://127.0.0.1:\(port)/<canvas_id>/
defer { kernel.shutdown() }
```

The kernel binds an axum HTTP / WS / REST server on `127.0.0.1:<port>`
inside the app process. Loopback TCP is permitted in iOS sandbox.
WKWebView consumes the same surface a browser does on a server — no
divergence between embedded and standalone runs.

## Wire-level surface

Same as the standalone CLI:

| route                                | what                              |
|--------------------------------------|-----------------------------------|
| `GET /`                              | root index                        |
| `GET /transport.js`                  | the JS transport client           |
| `GET /<agent_id>/`                   | render_html dispatch              |
| `GET /<agent_id>/file/<path>`        | file read proxy                   |
| `GET /<agent_id>/ws`                 | WebSocket verb channel            |
| `GET /<rest>/_reflect[/<target>]`    | REST reflect shortcut             |
| `POST /<rest>/<target>` body=json    | REST verb dispatch                |

## License

AGPL-3.0-or-later (matches the parent crate).
