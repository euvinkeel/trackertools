"""Local web server for the CoTracker3 subject-tracking editor.

Run:  .venv\\Scripts\\python.exe editor\\server.py   then open http://127.0.0.1:8000
"""

import asyncio
import hashlib
import json
import os
import sys
import threading
import time
import uuid
from pathlib import Path
from typing import Optional

import anyio
import uvicorn
from fastapi import FastAPI, HTTPException, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse, JSONResponse, Response
from fastapi.staticfiles import StaticFiles

EDITOR_DIR = Path(__file__).resolve().parent
REPO_ROOT = EDITOR_DIR.parent
sys.path.insert(0, str(REPO_ROOT))
sys.path.insert(0, str(EDITOR_DIR))

import cv2  # noqa: E402

from engine import Segment, TrackingEngine, probe_video  # noqa: E402
from frames import FrameServer, crop  # noqa: E402
from proxy import ProxyManager, needs_proxy  # noqa: E402
from bounds import parse_bounds  # noqa: E402
from runs import start_run  # noqa: E402
from templates import TemplateSegment  # noqa: E402

DATA_DIR = Path(os.environ.get("COTRACK_EDITOR_DATA", EDITOR_DIR / "data"))
VIDEO_DIR = DATA_DIR / "videos"
META_DIR = DATA_DIR / "meta"
PROJECT_DIR = DATA_DIR / "projects"
PROXY_DIR = DATA_DIR / "proxies"
for d in (VIDEO_DIR, META_DIR, PROJECT_DIR, PROXY_DIR):
    d.mkdir(parents=True, exist_ok=True)

app = FastAPI(title="CoTracker3 Editor")
engine: Optional[TrackingEngine] = None
engine_error: Optional[str] = None
frame_server = FrameServer()
MAX_CROP_PIXELS = 4096 * 4096


def _load_engine():
    global engine, engine_error
    try:
        t = time.time()
        eng = TrackingEngine(str(REPO_ROOT))
        eng.ready.wait()
        engine = eng
        print(f"[editor] model ready on {eng.device} in {time.time() - t:.1f}s")
    except Exception as exc:
        import traceback

        traceback.print_exc()
        engine_error = f"{type(exc).__name__}: {exc}"


threading.Thread(target=_load_engine, daemon=True).start()


# ---- video registry -------------------------------------------------------

def _meta_path(video_id: str) -> Path:
    if not video_id.replace("-", "").isalnum():
        raise HTTPException(400, "bad id")
    return META_DIR / f"{video_id}.json"


def load_meta(video_id: str) -> dict:
    p = _meta_path(video_id)
    if not p.exists():
        raise HTTPException(404, "unknown video")
    return json.loads(p.read_text(encoding="utf-8"))


def save_meta(meta: dict):
    _meta_path(meta["id"]).write_text(json.dumps(meta, indent=2), encoding="utf-8")


def _proxy_ready(video_id: str, proxy: dict):
    meta = load_meta(video_id)
    meta["proxy"] = proxy
    save_meta(meta)


proxies = ProxyManager(PROXY_DIR, probe_video, _proxy_ready)


def proxy_info(meta: dict) -> dict:
    p = meta.get("proxy")
    if p and p.get("state") == "ready" and Path(p["path"]).exists():
        return {k: v for k, v in p.items() if k != "path"}
    return proxies.status(meta["id"])


def ensure_proxy(meta: dict):
    if "needsProxy" not in meta or "fpsRational" not in meta:
        meta.update(probe_video(meta["path"]))
        meta["needsProxy"] = needs_proxy(meta)
        save_meta(meta)
    if meta.get("needsProxy") and proxy_info(meta)["state"] in ("none", "error"):
        proxies.request(meta)


def public_meta(meta: dict) -> dict:
    out = {k: v for k, v in meta.items() if k != "path"}
    out["hasProject"] = (PROJECT_DIR / f"{meta['id']}.json").exists()
    out["copied"] = meta.get("copied", False)
    out["sourcePath"] = None if meta.get("copied") else meta.get("path")
    out["proxy"] = proxy_info(meta)
    return out


def register(path: Path, name: str, fingerprint: Optional[str], copied: bool,
             video_id: Optional[str] = None) -> dict:
    info = probe_video(str(path))
    meta = {
        "id": video_id or uuid.uuid4().hex[:16],
        "name": name,
        "path": str(path),
        "fingerprint": fingerprint,
        "copied": copied,
        "size": path.stat().st_size,
        "addedAt": time.time(),
        "openedAt": time.time(),
        **info,
    }
    meta["needsProxy"] = needs_proxy(meta)
    save_meta(meta)
    ensure_proxy(meta)
    return meta


@app.get("/api/health")
def health():
    import torch

    return {
        "modelReady": engine is not None,
        "modelError": engine_error,
        "device": engine.device if engine else None,
        "gpu": torch.cuda.get_device_name(0) if torch.cuda.is_available() else None,
    }


@app.get("/api/videos")
def list_videos():
    metas = []
    for p in META_DIR.glob("*.json"):
        try:
            m = json.loads(p.read_text(encoding="utf-8"))
        except Exception:
            continue
        if Path(m["path"]).exists():
            metas.append(public_meta(m))
    metas.sort(key=lambda m: m.get("openedAt", 0), reverse=True)
    return metas[:30]


@app.get("/api/videos/lookup")
def lookup_video(fingerprint: str):
    for p in META_DIR.glob("*.json"):
        try:
            m = json.loads(p.read_text(encoding="utf-8"))
        except Exception:
            continue
        if m.get("fingerprint") == fingerprint and Path(m["path"]).exists():
            m["openedAt"] = time.time()
            save_meta(m)
            ensure_proxy(m)
            return public_meta(m)
    return None


@app.get("/api/videos/{video_id}")
def get_video(video_id: str):
    m = load_meta(video_id)
    m["openedAt"] = time.time()
    save_meta(m)
    ensure_proxy(m)
    return public_meta(m)


@app.get("/api/videos/{video_id}/proxy/status")
def proxy_status(video_id: str):
    return proxy_info(load_meta(video_id))


@app.get("/api/videos/{video_id}/proxy/file")
def proxy_file(video_id: str):
    p = load_meta(video_id).get("proxy")
    if not p or not Path(p["path"]).exists():
        raise HTTPException(404, "no proxy")
    return FileResponse(p["path"], media_type="video/mp4", content_disposition_type="inline")


@app.post("/api/videos/upload")
async def upload_video(request: Request, name: str, fingerprint: Optional[str] = None):
    ext = Path(name).suffix.lower() or ".mp4"
    video_id = uuid.uuid4().hex[:16]
    dest = VIDEO_DIR / f"{video_id}{ext}"
    tmp = dest.with_suffix(ext + ".part")
    buf = bytearray()
    try:
        with open(tmp, "wb") as f:
            async for chunk in request.stream():
                buf.extend(chunk)
                if len(buf) >= 8 * 1024 * 1024:
                    data, buf = bytes(buf), bytearray()
                    await anyio.to_thread.run_sync(f.write, data)
            if buf:
                await anyio.to_thread.run_sync(f.write, bytes(buf))
        tmp.replace(dest)
        meta = await anyio.to_thread.run_sync(
            lambda: register(dest, name, fingerprint, copied=True, video_id=video_id)
        )
    except Exception as exc:
        tmp.unlink(missing_ok=True)
        dest.unlink(missing_ok=True)
        raise HTTPException(400, f"Could not import video: {exc}")
    return public_meta(meta)


@app.post("/api/videos/open")
async def open_path(body: dict):
    raw = (body.get("path") or "").strip().strip('"')
    path = Path(raw).expanduser()
    if not path.is_file():
        raise HTTPException(400, f"File not found: {raw}")
    st = path.stat()
    fingerprint = "path:" + hashlib.sha1(
        f"{path.resolve()}|{st.st_size}|{st.st_mtime_ns}".encode()
    ).hexdigest()
    for p in META_DIR.glob("*.json"):
        m = json.loads(p.read_text(encoding="utf-8"))
        if m.get("fingerprint") == fingerprint:
            m["openedAt"] = time.time()
            save_meta(m)
            ensure_proxy(m)
            return public_meta(m)
    try:
        meta = await anyio.to_thread.run_sync(
            lambda: register(path, path.name, fingerprint, copied=False)
        )
    except Exception as exc:
        raise HTTPException(400, f"Could not read video: {exc}")
    return public_meta(meta)


@app.get("/api/videos/{video_id}/file")
def video_file(video_id: str):
    m = load_meta(video_id)
    return FileResponse(m["path"], filename=m["name"], content_disposition_type="inline")


@app.get("/api/videos/{video_id}/crop")
def video_crop(video_id: str, f: int, x: int, y: int, w: int, h: int, scale: float = 1.0):
    """PNG of an original-resolution region of frame f (off-frame parts are black)."""
    m = load_meta(video_id)
    if w <= 0 or h <= 0 or w * h > MAX_CROP_PIXELS or not (0.05 <= scale <= 16):
        raise HTTPException(400, "bad crop size")
    try:
        img = crop(frame_server.get(m, f, "rgb24"), x, y, w, h)
    except Exception as exc:
        raise HTTPException(500, f"Could not decode frame {f}: {exc}")
    if scale != 1.0:
        size = (max(1, round(w * scale)), max(1, round(h * scale)))
        img = cv2.resize(img, size, interpolation=cv2.INTER_AREA if scale < 1 else cv2.INTER_NEAREST)
    ok, png = cv2.imencode(".png", cv2.cvtColor(img, cv2.COLOR_RGB2BGR), [cv2.IMWRITE_PNG_COMPRESSION, 1])
    if not ok:
        raise HTTPException(500, "encode failed")
    return Response(png.tobytes(), media_type="image/png", headers={"Cache-Control": "private, max-age=86400"})


# ---- projects ---------------------------------------------------------------

@app.get("/api/projects/{video_id}")
def get_project(video_id: str):
    _meta_path(video_id)
    p = PROJECT_DIR / f"{video_id}.json"
    if not p.exists():
        return JSONResponse(None)
    return FileResponse(p, media_type="application/json")


@app.put("/api/projects/{video_id}")
async def put_project(video_id: str, request: Request):
    _meta_path(video_id)
    body = await request.body()
    json.loads(body)  # validate
    p = PROJECT_DIR / f"{video_id}.json"
    tmp = p.with_suffix(".json.tmp")
    await anyio.to_thread.run_sync(tmp.write_bytes, body)
    tmp.replace(p)
    return {"ok": True, "bytes": len(body)}


# ---- tracking websocket -------------------------------------------------------

@app.websocket("/api/track")
async def track_socket(ws: WebSocket):
    await ws.accept()
    loop = asyncio.get_running_loop()
    outbox: asyncio.Queue = asyncio.Queue()
    job = None
    run_id = None

    async def sender():
        while True:
            msg = await outbox.get()
            await ws.send_text(json.dumps(msg, separators=(",", ":")))

    send_task = asyncio.create_task(sender())
    try:
        while True:
            msg = await ws.receive_json()
            kind = msg.get("type")
            if kind == "start":
                run_id = msg.get("runId")
                if job is not None:
                    job.halt()
                raw = msg.get("segments", [])
                end = lambda s: None if s.get("end") is None else int(s["end"])  # noqa: E731
                sid = lambda s: None if s.get("subjectId") is None else int(s["subjectId"])  # noqa: E731
                points = [Segment(key=s["key"], q=int(s["q"]), x=float(s["x"]), y=float(s["y"]), end=end(s),
                                  subject_id=sid(s))
                          for s in raw if s.get("kind", "point") != "template"]
                templates = [TemplateSegment(key=s["key"], q=int(s["q"]), x=float(s["x"]), y=float(s["y"]),
                                             end=end(s), threshold=float(s.get("threshold", 0.7)),
                                             looks=s.get("looks") or [], subject_id=sid(s))
                             for s in raw if s.get("kind") == "template"]
                bounds = parse_bounds(msg.get("bounds"))
                if points and engine is None:
                    await outbox.put({"type": "status", "runId": run_id,
                                      "state": "error" if engine_error else "loading",
                                      "message": engine_error or "Model is still loading, try again in a moment."})
                    continue
                meta = load_meta(msg["videoId"])

                def emit(m, run_id=run_id):
                    m["runId"] = run_id
                    loop.call_soon_threadsafe(outbox.put_nowait, m)

                job = start_run(engine, meta, int(msg.get("startFrame", 0)), points, templates, frame_server, emit,
                                bounds)
            elif kind == "halt":
                if job is not None:
                    job.halt()
            elif kind == "retire":
                if job is not None and msg.get("runId") == run_id:
                    job.retire(msg.get("entries") or [])
    except WebSocketDisconnect:
        pass
    finally:
        if job is not None:
            job.halt()
        send_task.cancel()


# ---- static frontend -------------------------------------------------------------

STATIC_DIR = EDITOR_DIR / "static"


@app.get("/")
def index():
    return FileResponse(STATIC_DIR / "index.html", headers={"Cache-Control": "no-store"})


app.mount("/static", StaticFiles(directory=STATIC_DIR), name="static")


@app.middleware("http")
async def no_cache_static(request: Request, call_next):
    response = await call_next(request)
    if request.url.path.startswith("/static/"):
        response.headers["Cache-Control"] = "no-store"
    return response


if __name__ == "__main__":
    port = int(os.environ.get("COTRACK_EDITOR_PORT", "8000"))
    print(f"[editor] http://127.0.0.1:{port}")
    uvicorn.run(app, host="127.0.0.1", port=port, log_level="warning")
