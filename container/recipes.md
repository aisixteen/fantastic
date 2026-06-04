# Fantastic recipes — what you can assemble

These are **general recipes**, not scripts. Hand one to an LLM together with the
kernel's own self-description —

```text
POST /rest/kernel  {"type":"reflect","readme":true,"bundles":"all"}
```

— and it has everything it needs to assemble *something like* the recipe by
itself: `reflect` tells it the live tree, `bundles` tells it what it can
`create_agent` from, and each agent's `verbs` tell it how to call them. The
recipe gives the **goal + the agent tree + the wiring**; the model derives the
exact `create_agent` / `send` calls. That's the bar — capability *emerges* from
self-description, it isn't hand-coded.

Distilled from a real federated canvas (a spatial board whose tiles were live
sub-projects: a genomic transformer visualization, LoRA / diffusion **training**
pipelines, generative **audio-visual** panels, dev terminals).

## The one thing to know first — two kernels

Fantastic is **two cooperating kernels**, and recipes always split across them:

```text
HOST kernel (this container: python or rust)         FRONTEND kernel (browser, ts/)
  data · compute · transport · memory                  the VIEW lives here
  fs_loader(root) web web_ws web_rest file             canvas · *_view · *content
  python_runtime terminal_backend ai_* yaml_state      (federates to the host over
  scheduler kernel_bridge local_runner ssh_runner       the SAME web_ws WS wire)
```

The host **serves** the frontend (a static `file` agent) and **relays** the WS
bus — it renders no UI itself. Every panel/tile is a **frontend** agent that the
canvas iframes. Binding is **weak**: agents reference peers by **id** + duck-typed
verbs (`get_webapp`, `render_html`, `reload_html`), never by concrete type — so a
backend runs headless with no view, and a view attaches/detaches freely.

Three view contracts the canvas/host already understand:

- `get_webapp → {url, default_width, default_height, title}` — canvas iframes it as a tile.
- `render_html → {html}` — duck-typed panel body.
- `reload_html` event — any agent emits it to reload its connected view.
- static assets: any `file` agent serves `GET /<file_id>/file/<path>`.

> **Ported status.** The host bundles below all ship in this image. The frontend
> bundles (`canvas`, `terminal_view`, `html_agent`, `gl_agent`, `ai_view`,
> `loader`) ship in `ts/` (the embedded `js_kernel.zip`). What is **not** ported
> is the *app-specific content* (your shaders, your training UI, your synth) and a
> couple of wiring gotchas — each recipe flags them under **⚠ Today**.

---

## 1. Spatial canvas of panels (the substrate UI)

**Build:** a zoomable board where every tile is some agent's live UI; drag,
position, persist.

```text
HOST:      fs_loader(root) → web(:PORT) → web_ws, web_rest
                           → file "ts_dist"   (serves the frontend bundle)
                           → fs_loader root=.fantastic/web   (frontend's store)
FRONTEND:  canvas compositor + whatever content/view agents you add
```

**Wiring:** the canvas iframes **any** agent that answers `get_webapp`; tile
positions (`x`/`y`) live on the agent's record; the frontend persists itself back
to host disk via its `proxy_loader`. Add a panel → it appears; remove it → the
tile goes.

**Hand to an LLM:** *"Serve the frontend and open `/`; the canvas hydrates the
tree. Any agent I create that answers `get_webapp` becomes a tile."*

**⚠ Today:** fully ported. This is the base every other recipe builds on.

---

## 2. Terminal / dev console

**Build:** an xterm panel wired to a real PTY shell in a project dir (with
clipboard-image paste).

```text
HOST:      terminal_backend  (PTY; cwd = your project)
FRONTEND:  terminal_view     (xterm)  ── bound by id ──▶ the backend
```

**Wiring:** weak — the backend is headless; the view attaches by id. Streaming
flow-control (`ack` past ~100K unacked), incremental UTF-8 (no split-char litter),
serialized paste-safe writes, and an image-paste bridge (browser clipboard image
→ saved file → path typed into the shell, e.g. for a CLI like `claude`).

**Hand to an LLM:** *"Create a `terminal_backend` cwd'd at my project; attach a
`terminal_view`; give me a shell."*

**⚠ Today:** fully ported. The PTY runs **inside** this image (sh/bash); mount
your project at `/work` so the shell sees your files.

---

## 3. AI chat with tool-use (agents as tools, emergent routing)

**Build:** a chat panel to Claude / ollama / NVIDIA NIM; the model **calls other
agents as tools** and routes its own output.

```text
HOST:      ai backend (anthropic_backend | ollama_backend | nvidia_nim_backend)
             + file agent (chat history sidecar; file_agent_id=…)
FRONTEND:  ai_view (chat panel)  ── by id ──▶ the backend
```

**Wiring:** send a prompt to the backend; it streams `token`/`done` **on its own
id**; the prompt *names who listens* and the system prompt carries the `send()`
signature, so the model **routes its own result** (no `reply_to` primitive; 1:N
falls out). Tell it the ids of a `python_runtime`, a `file`, a `yaml_state` and it
calls them as tools. A recursion guard keeps AI→AI chains safe.

**Hand to an LLM:** *"Create an `anthropic_backend` (key from env); attach an
`ai_view`. Here are the ids of a python_runtime and a yaml_state memory — use them
as tools."*

**⚠ Today:** fully ported. Pass the key at launch (`-e ANTHROPIC_KEY=…`). Durable
memory = a `yaml_state` agent (recipe 4's sibling); the model decides what to save
and recalls it on a fresh turn — judgment emerges, it isn't coded.

---

## 4. Background compute / training job runner (+ live progress panel)

**Build:** launch a long Python job (training, preprocessing, a render) as a
background job, stream its progress to a live panel, manage it by `job_id`.
*(distilled from LoRA / denoiser-classifier / conditional-flow-matching training
boards.)*

```text
HOST:      python_runtime  (cwd = project)   ── start → job_id; streams progress/job_done
             + file agent  (serves the job's own UI / artifacts)
FRONTEND:  an html panel (or ai_view) subscribing to progress — or the job's
           own training_ui.html served via the file agent
```

**Wiring:** `python_runtime.start` runs `python -u …` in the background (many in
parallel), returns a `job_id` immediately, streams `progress`/`job_done`;
`status`/`stop`/`interrupt`/`clear` by `job_id`. The spawned code gets a **kernel
connector** (send/emit/spawn back into the kernel over a socketpair), so a job can
push frames to a panel, call an AI, or read memory — by id.

**Hand to an LLM:** *"Create a `python_runtime` at my project; start `train.py` as
a job; render its `progress` stream into an html panel and serve `training_ui.html`
via a file agent."*

**⚠ Today:** runner ported. **GPU is the host's** — this image is CPU-only by
design, so heavy training runs on a GPU host (native or a GPU-base image), with the
panel/board the same either way.

---

## 5. Live data / WebGL visualization panel

**Build:** a live WebGL panel rendering data a Python job produces (point clouds,
Gaussians, an attention/search-space collapse). *(distilled from a genomic
diffusion-transformer visualization: a looping point-cloud → Gaussian → collapse
motif beside a manim animation.)*

```text
HOST:      python_runtime (computes frames/point-clouds) + file agent (serves js/assets)
FRONTEND:  gl_agent (WebGL content)  ── fed by the host job; iframed by canvas
```

**Wiring:** `gl_agent` renders the shader/point-cloud; the host job streams data to
it by id; static assets via `/<file>/file/<path>`. A separate "explainer" animation
(e.g. manim) is just a plain `python_runtime` **render job** — no special agent.

**Hand to an LLM:** *"Create a `gl_agent` with my point-cloud shader; feed it frames
from a `python_runtime` job; drop it on the canvas."*

**⚠ Today:** `gl_agent` **is** ported (frontend). **Not** ported: your specific
shaders/`*.js` (app content — you supply them), and **headless/CI WebGL is
disabled** — a GL panel needs a real browser+GPU, not the test-headless path.

---

## 6. Generative audio-visual / media panel

**Build:** a generative AV panel (WebAudio synth + WebGL visuals), or an
audio-capture/playback tool, driven by a Python media backend. *(distilled from a
generative "trance" panel and audio + geo-data boards.)*

```text
HOST:      python_runtime (audio gen/processing; writes wavs/geojson) + file agent (serves media)
           [+ a HOST "bus" agent if multiple panels must talk]
FRONTEND:  html_agent / gl_agent panel (WebAudio + canvas/WebGL)
```

**Wiring:** the panel synthesizes/visualizes; the host produces + serves artifacts
(`/<file>/file/audio/…`). The panel plays generated audio and reacts.

**Hand to an LLM:** *"Create an html panel with my WebAudio synth; serve its
generated audio via a file agent; if a second panel needs to sync, route their
messages through a host bus agent."*

**⚠ Today (the gotchas):** WebAudio autoplay needs the iframe `allow="autoplay"`
(the html view sets this). **Cross-panel messaging must route through a HOST bus
agent** — `BroadcastChannel` does **not** cross iframes/origins reliably; that bus
agent is **wiring you add**, not a default bundle. App content (the synth/visual)
is yours to supply.

---

## 7. Federated multi-project canvas (many kernels, one board)

**Build:** one board/brain hosting many **independent** projects — each its *own*
`fantastic` kernel — addressed by id, wired across kernels. *(this is the whole
original setup: a canvas of `local_runner` tiles, one per project folder.)*

```text
HOST (brain): fs_loader → web ;  per project: local_runner (local dir) or
              ssh_runner (remote host) ;  kernel_bridge per peer
FRONTEND:     canvas — one tile per project, iframing that project's own webapp
```

**Wiring:** `local_runner` start/stop/status a project's own `fantastic` (truth from
its `.fantastic/lock.json` pid+port); `kernel_bridge` dials a peer **by URL** (weak
binding — memory / ws / ssh+ws transports); from **either** kernel a routine reads
memory anywhere + spawns ai/py **by id**. Each project can also be a **container** —
**a unit at `host:port`** (no shared network): the bridge dials
`host.containers.internal:<port>` for another container on this host, or
`ws://<ip>:<port>/…` (or the `ssh+ws` transport) for one on another machine.

**Hand to an LLM:** *"For each project dir, create a `local_runner` (or run its
container); bridge to it; iframe its webapp as a canvas tile. To reach a project on
another machine, bridge to its `ip:port`."*

**⚠ Today:** fully ported (`local_runner`, `ssh_runner`, `kernel_bridge`) — and the
container unit-model is the distribution shape: one self-describing image-with-head
per project, federated by URL.

---

### How to use these with this container

1. Run the image (head on by default): `GET /` is the descriptive head, and one
   `reflect` round-trip returns the full self-description.
2. Pick a recipe, paste it to an LLM **with** the `reflect readme=true bundles=all`
   output, and ask it to assemble it.
3. It composes the host agents via `create_agent`, serves + opens the frontend, and
   wires the panels by id. You get a working approximation — a quickstart — to push
   further. Mount your project at `/work` so file/terminal/python agents see it.
