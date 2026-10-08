"""TAPNext++ (editor/tapnext/) on the sprite fixture, on the CPU only (never
the graphics card: the app may be using it).

1. In process: a query on a later frame (a reset point) through one online
   state gives what the offline model gives over the whole clip, so reset
   points need no state of their own.
2. Through the shared worker (editor/cotracker_worker.py --shared): a
   TAPNext stream tracking the sprite from the first frame and from a reset
   point 20 frames in, against its known path; beside it, a stream asking for
   an unknown method fails alone.

Crops are 512 × 384 around a rough guide (the line between the clip's
ends: the sprite wanders in the crop, as it does around the app's guide).
Skips without torch, the weights (TT_TAPNEXT_WEIGHTS, and CoTracker3's for
the worker: torch hub's cache or TT_COTRACKER_WEIGHTS) or the fixture
(`cargo xtask fixtures`, or TT_FIXTURES).

Run: .venv\\Scripts\\python.exe editor\\tests\\test_tapnext.py
"""
import json
import math
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

os.environ["CUDA_VISIBLE_DEVICES"] = "-1"  # before torch loads: the CPU only

import numpy as np

EDITOR = Path(__file__).resolve().parents[1]
WORKER = EDITOR / "cotracker_worker.py"
sys.path.insert(0, str(EDITOR))
W, H = 512, 384
F0, F1 = 570, 630  # the clip (fixture frames)
RESET = 20  # the reset point's frame in the clip


def skip(why):
    print(f"skipped: {why}")
    sys.exit(0)


try:
    import av
    import torch
except ImportError:
    skip("no torch or PyAV")
weights = os.environ.get("TT_TAPNEXT_WEIGHTS")
if not weights or not os.path.exists(weights):
    skip(f"no TAPNext++ weights (TT_TAPNEXT_WEIGHTS: {weights})")
if os.environ.get("TT_FIXTURES"):
    fixture = Path(os.environ["TT_FIXTURES"]) / "sprite_1080p60.mp4"
else:
    fixture = next((d / "fixtures" / "sprite_1080p60.mp4" for d in EDITOR.parents if (d / "fixtures" / "sprite_1080p60.mp4").exists()), None)
if not fixture or not fixture.exists():
    skip("sprite_1080p60.mp4 not found (cargo xtask fixtures, or set TT_FIXTURES)")


def truth(f):
    """The sprite's centre on fixture frame f (as crates/tt_track/tests/cotracker_queue.rs)."""
    t = f / 60
    p = [950 + 500 * math.sin(0.9 * t) + 60 * math.sin(5.3 * t), 530 + 300 * math.sin(1.3 * t + 0.7) + 40 * math.sin(4.1 * t)]
    return np.array([2 * math.floor(math.floor(v) / 2) + 10.5 for v in p])


def clip():
    """[(crop, its origin in the frame)] for frames F0..F1."""
    out = []
    with av.open(str(fixture)) as c:
        for i, fr in enumerate(c.decode(video=0)):
            if i > F1:
                break
            if i >= F0:
                a = (i - F0) / (F1 - F0)
                g = truth(F0) * (1 - a) + truth(F1) * a
                x0, y0 = int(round(g[0] - W / 2)), int(round(g[1] - H / 2))
                img = fr.to_ndarray(format="rgb24")
                out.append((np.ascontiguousarray(img[y0:y0 + H, x0:x0 + W]), np.array([x0, y0])))
    return out


crops = clip()
seeds = [[0, *(truth(F0) - crops[0][1])], [RESET, *(truth(F0 + RESET) - crops[RESET][1])]]

# 1. A reset point online = offline (a short clip: the offline pass holds it all).
torch.set_grad_enabled(False)
from tapnext.tracker import Model, Stream  # noqa: E402

model = Model(weights, "cpu")
n = 16
out = []
s = Stream(model, {"width": W, "height": H, "queries": [seeds[0], [8, *(truth(F0 + 8) - crops[8][1])]], "support": 0}, out.append)
for img, _ in crops[:n]:
    s.window([img], False)
video = torch.cat([model.prepare(img) for img, _ in crops[:n]], dim=1)
tracks, _, vis, _ = model.net(video=video, query_points=s.query_tensor)
offline = (tracks[0].flip(-1) * s.scale).numpy()
for f, msg in enumerate(out):
    for k, q in enumerate((0, 8)):
        p = msg["points"][k]
        assert (p is None) == (f < q), (f, k, p)
        if f > q:
            assert np.hypot(*(np.array(p[:2]) - offline[f, k])) < 0.01, (f, k, p, offline[f, k])
            assert abs(p[2] - float(torch.sigmoid(vis[0, f, k, 0]))) < 1e-4
print(f"reset point: online = offline over {n} frames")
del model, s, video, tracks

# 2. Through the shared worker.
env = {**os.environ, "CUDA_VISIBLE_DEVICES": "-1", "TT_COTRACKER_WARM": "0", "TT_TAPNEXT_WEIGHTS": weights}
proc = subprocess.Popen([sys.executable, str(WORKER), "--shared", "--device", "cpu"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env)
ready = proc.stdout.readline()
if b"error" in ready and b"CoTracker3 weights" in ready:
    skip("no CoTracker3 weights for the worker")
assert b"ready" in ready, ready


def message(tag, sid, payload=b""):
    proc.stdin.write(tag + sid.to_bytes(4, "little") + payload)


def feed():
    """Written beside the reading below: the worker's replies would fill
    the pipe (4 KB on Windows) while it waits for us to take them."""
    message(b"O", 1, (json.dumps({"width": W, "height": H, "method": "tapnext", "queries": seeds}) + "\n").encode())
    message(b"O", 2, (json.dumps({"width": W, "height": H, "method": "nope", "queries": seeds}) + "\n").encode())
    for img, _ in crops:
        message(b"F", 1, img.tobytes())
        message(b"F", 2, img.tobytes())  # read and dropped
    message(b"E", 1)
    message(b"E", 2)
    proc.stdin.close()


frames, errors, failed, done = [], [[], []], False, False
t0 = time.time()
threading.Thread(target=feed, daemon=True).start()
for line in proc.stdout:
    msg = json.loads(line)
    if msg["s"] == 2:
        assert "unknown method" in msg.get("error", ""), msg
        failed = True
        continue
    assert "error" not in msg, msg
    if msg.get("done"):
        done = True
        continue
    f = msg["f"]
    frames.append(f)
    tr = truth(F0 + f) - crops[f][1]
    for k, p in enumerate(msg["points"]):
        assert (p is None) == (f < seeds[k][0]), (f, k, p)
        if p is not None and f > seeds[k][0]:
            assert 0 <= p[2] <= 1
            errors[k].append(np.hypot(p[0] - tr[0], p[1] - tr[1]))
assert proc.wait() == 0
assert failed and done and frames == list(range(len(crops))), (failed, done, frames)
for k, name in enumerate(("from the start", f"from frame {RESET}")):
    e = np.array(errors[k])
    print(f"{name}: {len(e)} frames, error median {np.median(e):.2f} px, mean {e.mean():.2f}, max {e.max():.2f}")
    assert np.median(e) < 2.0 and e.max() < 8.0, name
print(f"worker: {len(crops)} frames in {time.time() - t0:.0f} s")
print("ok")
