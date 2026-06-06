# Security Policy

## Supported versions

Aisixteen Fantastic (`fantastic-canvas`) is **pre-1.0 (alpha)**. Security fixes
land on the **latest release line only**; older tags are not maintained.

| Version            | Supported          |
| ------------------ | ------------------ |
| latest `v0.5.x`    | :white_check_mark: |
| any earlier tag    | :x:                |

## Reporting a vulnerability

**Please do not open a public issue, PR, or discussion for a security problem.**

Use GitHub's **private vulnerability reporting**: open the repository's
**Security** tab → **Report a vulnerability**. That creates a private advisory
visible only to the maintainers, where we can triage and coordinate a fix and
disclosure with you.

Helpful to include, where you can:

- the affected **runtime(s)** — `python/`, `rust/`, `swift/`, or the `ts/`
  frontend — and a version, commit SHA, or image tag (`:vX.Y.Z-<arch>`);
- a description, impact, and a **minimal reproduction**;
- whether it touches the **protocol surface** (the HTTP/WS/REST wire shapes,
  system verbs, the `reflect` contract, or the on-disk `.fantastic/` format) —
  those affect **every** host runtime, so they're higher severity.

This is a best-effort alpha project: we aim to acknowledge within a few business
days and will agree a remediation + disclosure timeline with the reporter.

## Threat model & scope (please read before reporting)

Fantastic is a **single-operator** system: one user's **fleet** of kernels that
**mutually trust** each other. It is **not multi-tenant** and does not try to
isolate untrusted callers from each other.

Consequently the kernels' control surfaces — **HTTP / WebSocket / REST** — are
**not independently authenticated by design**. The trust boundary is the
**channel / transport**: loopback, an SSH-tunnelled or otherwise authenticated
broker, or a trusted private network. Anyone who can reach a kernel's control
surface can drive it — including stopping it via the `shutdown_kernel` verb. That
is intended; deploy accordingly:

- bind to loopback (`-p 127.0.0.1:<port>:<port>`) or front the kernel with an
  authenticated proxy / SSH tunnel;
- treat container kernels as **disposable execution units** — never bake or
  persist credentials in the image **or** in the bind-mounted `.fantastic/`
  workdir; inject **scoped, short-lived** tokens via env at spawn time;
- the spawning control plane (e.g. the AI chat that launched the kernel) holds
  the credentials — the kernel does not.

A report that simply states "the WS/REST surface has no auth" describes the
documented model, **not a vulnerability**. Reports that *defeat* the intended
boundary are in scope — for example: path-traversal past a `file` agent's root,
sandbox escape from a view iframe, a crafted payload that crashes or hangs the
kernel (DoS), secrets leaking into `reflect`/logs/`.fantastic/`, or a weak-load /
deserialization flaw in the on-disk format.

**In scope:** the kernels (`python/`, `rust/`, `swift/`), the browser frontend
(`ts/`), the universal container image, and the `send`/`reflect` protocol +
`.fantastic/` on-disk format.

**Out of scope:** vulnerabilities in third-party dependencies (report upstream;
tell us if a fix needs a version bump here), and any separately-licensed managed
cloud / relay / sync offering (not part of this repository).
