"""Does the bounds starting guess help CoTracker on fast motion?

Builds a synthetic clip (textured patch moving fast over a still photo), tracks
9 points on the patch with and without a rough bounds guide, and compares the
error against the known path.

Run: .venv\\Scripts\\python.exe editor\\tests\\bench_bounds_guide.py
"""
import base64
import os
import sys
import threading
from pathlib import Path

import av
import cv2
import numpy as np

EDITOR = Path(__file__).resolve().parents[1]
REPO = EDITOR.parent
sys.path.insert(0, str(EDITOR))
from bounds import parse_bounds  # noqa: E402
from engine import Segment, TrackingEngine, probe_video  # noqa: E402

TMP = Path(os.environ.get("LOCALAPPDATA", "")) / "Temp" / "opencode"
# Source footage for the synthetic clip, vendored with the tests.
SRC_VIDEO = Path(__file__).resolve().parent / "data" / "paragliding-launch.mp4"
if not SRC_VIDEO.exists():  # fall back to a co-tracker checkout, if present
    SRC_VIDEO = REPO / "gradio_demo" / "videos" / "paragliding-launch.mp4"
SPEED = float(sys.argv[1]) if len(sys.argv) > 1 else 55  # max velocity component, px/frame
CLIP = TMP / f"fast_patch_{int(SPEED)}.mp4"
W, H, FPS, N, P = 1280, 720, 30, 240, 128


def make_path():
    rng = np.random.default_rng(3)
    pos = [np.array([300.0, 360.0])]
    vel = np.array([30.0, 10.0]) * SPEED / 55
    for f in range(1, N):
        if f % 12 == 0:
            vel = rng.uniform(-1, 1, 2) * SPEED
        p = pos[-1] + vel
        for i, lim in ((0, W), (1, H)):  # bounce off the edges
            if p[i] < P or p[i] > lim - P:
                vel[i] = -vel[i]
                p[i] = np.clip(p[i], P, lim - P)
        pos.append(p)
    return np.array(pos)


def make_clip(path):
    src = av.open(str(SRC_VIDEO))
    frames = [fr.to_ndarray(format="rgb24") for _, fr in zip(range(40), src.decode(video=0))]
    src.close()
    bg = cv2.resize(frames[0], (W, H), interpolation=cv2.INTER_AREA)
    tex = cv2.resize(frames[39], (W, H), interpolation=cv2.INTER_AREA)[300:300 + P, 500:500 + P].copy()
    cv2.rectangle(tex, (8, 8), (P - 9, P - 9), (255, 255, 255), 3)
    cv2.circle(tex, (P // 2, P // 2), 20, (255, 40, 40), -1)
    out = av.open(str(CLIP), "w")
    st = out.add_stream("libx264", rate=FPS)
    st.width, st.height, st.pix_fmt = W, H, "yuv420p"
    st.options = {"crf": "16", "g": "15"}
    for c in path:
        img = bg.copy()
        x0, y0 = int(round(c[0])) - P // 2, int(round(c[1])) - P // 2
        img[y0:y0 + P, x0:x0 + P] = tex
        for pkt in st.encode(av.VideoFrame.from_ndarray(img, format="rgb24")):
            out.mux(pkt)
    for pkt in st.encode():
        out.mux(pkt)
    out.close()


def run(engine, meta, path, bounds):
    offsets = [(dx, dy) for dx in (-40, 0, 40) for dy in (-40, 0, 40)]
    c0 = np.round(path[0])
    segs = [Segment(key=str(i), q=0, x=c0[0] + dx + 0.5, y=c0[1] + dy + 0.5, subject_id=1) for i, (dx, dy) in enumerate(offsets)]
    res = {}
    done = threading.Event()

    def emit(m):
        if m["type"] == "results":
            for key, it in m["items"].items():
                d = it["data"]
                for i in range(len(d) // 3):
                    res[(int(key), it["f0"] + i)] = d[3 * i: 3 * i + 2]
        elif m["type"] == "status" and m["state"] in ("done", "error", "halted"):
            done.set()

    engine.start_job(meta, 0, segs, emit, bounds)
    done.wait(300)
    errs = []
    for (i, f), (x, y) in res.items():
        c = np.round(path[f])
        errs.append(np.hypot(x - (c[0] + offsets[i][0] + 0.5), y - (c[1] + offsets[i][1] + 0.5)))
    errs = np.array(errs)
    return np.median(errs), np.mean(errs), (errs > 16).mean()


def rough_bounds(path):
    # A deliberately rough box: 1.8x the patch, center jittered and lagging a frame.
    rng = np.random.default_rng(5)
    data = np.zeros((N, 4), np.float32)
    lag = np.vstack([path[:1], path[:-1]])
    data[:, :2] = 0.5 * (path + lag) + rng.normal(0, 8, (N, 2))
    data[:, 2:] = P * 1.8
    return {"1": {"f0": 0, "data": base64.b64encode(data.tobytes()).decode(), "guide": True}}


if __name__ == "__main__":
    path = make_path()
    if not CLIP.exists():
        make_clip(path)
    meta = probe_video(str(CLIP))
    meta["path"] = str(CLIP)
    speed = np.hypot(*np.diff(path, axis=0).T)
    print(f"clip {W}x{H}, {N} frames, speed median {np.median(speed):.0f} px/frame, max {speed.max():.0f}")
    engine = TrackingEngine(str(REPO))
    engine.ready.wait()
    base = run(engine, meta, path, {})
    guided = run(engine, meta, path, parse_bounds(rough_bounds(path)))
    print("no guide     median %.2f px, mean %.2f px, >16 px on %.1f%% of samples" % (base[0], base[1], 100 * base[2]))
    print("bounds guide median %.2f px, mean %.2f px, >16 px on %.1f%% of samples" % (guided[0], guided[1], 100 * guided[2]))
