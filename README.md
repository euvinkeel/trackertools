# trackertools

A local, editor-agnostic **subject tracker** for video, built on CoTracker3. Drop
in a clip, define subjects with points and/or template patterns, let the model
track them in the background, correct it by hand where it fails, and export
positions for any NLE (Resolve, Premiere, …). The application is the *CoTrack
editor*.

This repository is self-contained: the CoTracker3 model code it runs is vendored
here, so no separate checkout is needed.

## Layout

```
trackertools/
├─ editor/          the application — FastAPI server, browser UI, tests
│  ├─ server.py     entry point (also engine.py, templates.py, proxy.py, …)
│  ├─ static/       the front-end (HTML/CSS/JS, no build step)
│  ├─ tests/        Node model tests + Python/Python-Playwright tests
│  ├─ README.md     full workflow and export-format documentation
│  └─ PLAN.md       design notes and progress
├─ cotracker/       vendored CoTracker3 model code (installed editable)
├─ hubconf.py       vendored torch.hub entry point for the model
├─ setup.py         vendored package metadata for the editable install
├─ LICENSE.md       license of the vendored CoTracker3 code and weights
├─ requirements.txt runtime dependencies
└─ .venv/           local Python environment (not tracked)
```

## Run

```
.venv\Scripts\python.exe editor\server.py
```

Open http://127.0.0.1:8000. The model loads in the background (~10 s); you can
open a video meanwhile. Set `COTRACK_EDITOR_PORT` to change the port.

On first run the model weights download to
`%USERPROFILE%\.cache\torch\hub\checkpoints\scaled_online.pth`.

## Setup from scratch

```powershell
# 1. Python environment
py -3.12 -m venv .venv

# 2. Vendored CoTracker3 package (editable, from this repo) + runtime deps
.venv\Scripts\python.exe -m pip install -e .
.venv\Scripts\python.exe -m pip install -r requirements.txt

# 3. A CUDA build of torch matching your driver (example: CUDA 12.6)
.venv\Scripts\python.exe -m pip install torch==2.14.0+cu126 --index-url https://download.pytorch.org/whl/cu126
```

## Tests

From `editor/`:

```
npm test                       # Node model/unit tests (no install needed)
```

With the repo venv (from the repository root):

```
.venv\Scripts\python.exe editor\tests\test_template_bounds.py
.venv\Scripts\python.exe editor\tests\test_stored_pattern.py
```

The `editor/tests/ui_*.py` scripts are Playwright browser tests and expect a
running editor. See `editor/README.md` for the full list and `editor/PLAN.md`
for design details.

## Licensing

- The application code in `editor/` is this project's own code.
- The vendored `cotracker/` package, `hubconf.py`, `setup.py` and `LICENSE.md`
  are from [facebookresearch/co-tracker](https://github.com/facebookresearch/co-tracker).
- CoTracker3 **model weights** (downloaded on first run) are licensed
  **CC-BY-NC 4.0 — non-commercial use only**. See `LICENSE.md`.
