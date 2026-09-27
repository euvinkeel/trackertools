"""Experiment (Phase E): does tracking on a moving crop beat full-frame tracking?

The engine resizes the whole frame to 512x384, so a small subject (the synthetic
cursor is ~20x24 px in 1920x1080) lands on ~5x8 model pixels. A crop that follows
the subject's rough bounds and is resized to 512x384 gives it ~40x50 model pixels
instead. The trade-off: the crop loses global context, so recoveries (hidden
frames, a teleport) depend on the rough bounds being there.

This script measures both on the synthetic cursor clip with the same seed and a
simulated human bounds trace (10-frame lag + small jitter). It prints a table and
exits; nothing is exposed in the UI.

Run: .venv\\Scripts\\python.exe editor\\tests\\bench_crop.py
"""
import json
import math
import os
import sys
import threading
import time
from pathlib import Path

import cv2
import numpy as np

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "editor"))
import torch  # noqa: E402

from engine import MODEL_H, MODEL_W, Segment, TrackingEngine, Track  # noqa: E402
from frames import FrameReader  # noqa: E402

TMP = Path(os.environ.get("LOCALAPPDATA", "")) / "Temp" / "opencode"
CLIP_NAME = os.environ.get("COTRACK_BENCH_CLIP", "cursor_slow")
CLIP = TMP / f"{CLIP_NAME}.mp4"
TRUTH_PATH = TMP / f"{CLIP_NAME}_truth.json"
if not CLIP.exists() or not TRUTH_PATH.exists():
    CLIP, TRUTH_PATH = TMP / "cursor_clip.mp4", TMP / "cursor_truth.json"
TRUTH = json.loads(TRUTH_PATH.read_text())
FPS = float(TRUTH["fps"])
T0 = 0.0
N = len(TRUTH["tips"])
HIDDEN = set(range(*TRUTH["hidden"])) if TRUTH.get("hidden") else set()
raw = TRUTH["tips"]
known = [f for f, v in enumerate(raw) if v is not None]
TIPS = np.stack([np.interp(np.arange(N), known, [raw[f][i] for f in known]) for i in (0, 1)], 1) + 0.5
# Seed on the sprite's body (the hotspot offset the template editor uses), not
# the bounding-box center: the arrow's notch leaves the center on background.
CENTER = TIPS + np.array([2.5, 2.5])
META = {"id": "bench", "path": str(CLIP), "fps": FPS, "t0": T0, "width": 1920, "height": 1080, "frameCount": N}
CROP_SCALE = 3.0
CROP_MIN = 160.0


def bands():
    if HIDDEN:
        h0, h1 = min(HIDDEN), max(HIDDEN) + 1
        return [(0, 90, "0-89 steady"), (90, h0, f"90-{h0 - 1} flicks"),
                (h1, 400, f"{h1}-399 after hidden"), (400, N, f"400-{N - 1} after teleport")]
    step = N // 3
    return [(0, step, f"0-{step - 1}"), (step, 2 * step, f"{step}-{2 * step - 1}"), (2 * step, N, f"{2 * step}-{N - 1}")]


def rough_box(f: int):
    """A simulated Puppeteer trace: the truth 10 frames earlier, jittered. The
    crop keeps the model's 4:3 aspect so scaling is uniform."""
    g = max(0, f - 10)
    cx = float(TIPS[g][0] + 5 * math.sin(g * 0.13))
    cy = float(TIPS[g][1] + 4 * math.cos(g * 0.09))
    side = CROP_MIN
    return cx, cy, side, side * MODEL_H / MODEL_W


def crop_model(frame, box):
    """Full frame -> crop -> model-resolution RGB."""
    img = frame.to_ndarray(format="rgb24")
    x0, y0, x1, y1 = box
    x0, y0 = max(0, int(x0)), max(0, int(y0))
    x1, y1 = min(img.shape[1], int(x1)), min(img.shape[0], int(y1))
    sub = img[y0:y1, x0:x1]
    return cv2.resize(sub, (MODEL_W, MODEL_H), interpolation=cv2.INTER_AREA)


def full_frame_run(eng):
    out = {}

    def emit(m):
        if m.get("type") == "results":
            for it in m["items"].values():
                d = it["data"]
                for i in range(len(d) // 3):
                    out[it["f0"] + i] = (d[3 * i], d[3 * i + 1], d[3 * i + 2])

    job = eng.start_job(META, 0, [Segment(key="full", q=0, x=float(CENTER[0][0]), y=float(CENTER[0][1]), end=N)],
                        emit, None)
    job.done.wait(300)
    return out


def crop_run(eng):
    """Moving crop with the engine's rolling scheme: windows of S frames,
    stepping by S//2, chaining the overlap predictions (re-mapped between crop
    coordinate frames) instead of restarting the track every window."""
    out = {}
    S, step, overlap = eng.S, eng.step, eng.overlap
    pos = np.array(CENTER[0], float)
    f = 0
    first = True
    prev_src = None  # (overlap, 2) source-space predictions of the overlap frames
    prev_vis = None
    prev_conf = None
    feats = None
    with torch.inference_mode():
        while f < N:
            cx, cy, side_x, side_y = rough_box(f)
            x0 = cx - side_x / 2
            y0 = cy - side_y / 2
            box = (x0, y0, x0 + side_x, y0 + side_y)
            reader = FrameReader(META["path"], FPS, T0, f, threading.Event(),
                                 convert=lambda frame, b=box: crop_model(frame, b), maxsize=32)
            try:
                frames = [img for _, img in reader.read_indexed(min(S, N - f))]
            finally:
                reader.stop_event.set()
            if not frames:
                break
            if len(frames) < S:
                frames = frames + [frames[-1]] * (S - len(frames))
            pyramid = eng.encode(frames)
            track = Track(key="crop", q=f, x=0.0, y=0.0, end=f + S)
            if first:
                track.x = float((pos[0] - x0) * MODEL_W / side_x)
                track.y = float((pos[1] - y0) * MODEL_H / side_y)
                eng.sample_feats(pyramid, [track], 0)
                feats = track.feats
                first = False
            else:
                track.feats = feats
                # Re-map the overlap predictions into this window's crop space
                # (stride units for the engine).
                px = (prev_src[:, 0] - x0) * MODEL_W / side_x / eng.stride
                py = (prev_src[:, 1] - y0) * MODEL_H / side_y / eng.stride
                track.prev_coords = torch.tensor(np.stack([px, py], 1), device=eng.device, dtype=torch.float32)
                track.prev_vis = prev_vis
                track.prev_conf = prev_conf
            coords, vis, conf = eng.run_window(pyramid, [track])
            coords = coords.float().cpu().numpy()
            n_out = min(S, N - f)
            for i in range(n_out):
                sx = x0 + float(coords[i, 0, 0]) * side_x / MODEL_W
                sy = y0 + float(coords[i, 0, 1]) * side_y / MODEL_H
                out[f + i] = (sx, sy, 1.0)
            lo = max(0, S - overlap)
            prev_src = np.array([[x0 + float(coords[i, 0, 0]) * side_x / MODEL_W,
                                  y0 + float(coords[i, 0, 1]) * side_y / MODEL_H] for i in range(lo, S)])
            prev_vis = vis[lo:, 0]
            prev_conf = conf[lo:, 0]
            if n_out < S:
                break
            f += step
    return out


def summary(name, out):
    print(name)
    for lo, hi, label in bands():
        errs = []
        tracked = 0
        for f in range(lo, hi):
            v = out.get(f)
            if v is None:
                continue
            tracked += 1
            if f in HIDDEN:
                continue
            errs.append(math.hypot(v[0] - CENTER[f][0], v[1] - CENTER[f][1]))
        if not errs:
            print(f"  {label:<22} no data")
            continue
        errs = np.array(errs)
        print(f"  {label:<22} {tracked:3d}/{hi - lo} tracked · median {np.median(errs):6.2f} px · "
              f"mean {errs.mean():6.2f} px · p95 {np.percentile(errs, 95):7.2f} px")


def main():
    t0 = time.time()
    eng = TrackingEngine(str(REPO))
    eng.ready.wait()
    print(f"model ready on {eng.device} in {time.time() - t0:.1f}s")
    t = time.time()
    full = full_frame_run(eng)
    print(f"full-frame run in {time.time() - t:.1f}s")
    t = time.time()
    crop = crop_run(eng)
    print(f"moving-crop run in {time.time() - t:.1f}s")
    print()
    summary("full frame", full)
    summary("moving crop", crop)
    print("\nNote: the crop is 3x the rough box (min 160 px) and follows a simulated")
    print("human trace (10-frame lag + jitter); it trades global context for detail.")


if __name__ == "__main__":
    main()
