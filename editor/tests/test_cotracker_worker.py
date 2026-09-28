"""The CoTracker3 worker for trackertools v2 (editor/cotracker_worker.py),
driven the way tt_track's jobs drive it: a textured blob on a known path,
sent as 512 × 384 crops over its protocol. Skips without torch or the
weights (scaled_online.pth in torch hub's cache, or TT_COTRACKER_WEIGHTS).

Run: .venv\\Scripts\\python.exe editor\\tests\\test_cotracker_worker.py
"""
import json
import os
import subprocess
import sys
from pathlib import Path

import numpy as np

WORKER = Path(__file__).resolve().parents[1] / "cotracker_worker.py"
W, H = 512, 384

try:
    import torch

    weights = os.environ.get("TT_COTRACKER_WEIGHTS") or os.path.join(torch.hub.get_dir(), "checkpoints", "scaled_online.pth")
    if not os.path.exists(weights):
        print(f"skipped: no weights at {weights}")
        sys.exit(0)
except ImportError:
    print("skipped: no torch")
    sys.exit(0)


def path(f):
    return 150 + 4.0 * f, 200 + 30 * np.sin(f * 0.1)


def frame(c):
    y, x = np.mgrid[0:H, 0:W] + 0.5
    bg = 110 + 40 * np.sin(x * 0.05) * np.cos(y * 0.04)
    blob = 120 * np.exp(-((x - c[0]) ** 2 + (y - c[1]) ** 2) / 72) - 80 * np.exp(-((x - c[0] - 5) ** 2 + (y - c[1] + 4) ** 2) / 18)
    g = np.clip(bg + blob, 0, 255).astype(np.uint8)
    return np.stack([g, np.clip(g * 0.8 + 30, 0, 255).astype(np.uint8), g], -1)


N = 40
proc = subprocess.Popen([sys.executable, str(WORKER), "--device", "cpu"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
assert "ready" in json.loads(proc.stdout.readline())
# Two seeds: the blob from frame 0, and again from frame 20 (a later look).
header = {"width": W, "height": H, "queries": [[0, *path(0)], [20, *path(20)]]}
proc.stdin.write((json.dumps(header) + "\n").encode())
for f in range(N):
    proc.stdin.write(b"F" + frame(path(f)).tobytes())
proc.stdin.write(b"E")
proc.stdin.close()

seen, errors, done = [], [], False
for line in proc.stdout:
    msg = json.loads(line)
    assert "error" not in msg, msg
    if msg.get("done"):
        done = True
        continue
    f, points = msg["f"], msg["points"]
    seen.append(f)
    assert len(points) == 2
    assert (points[1] is None) == (f < 20), f"frame {f}: the second seed starts on frame 20"
    for p in points:
        if p is not None:
            errors.append(np.hypot(p[0] - path(f)[0], p[1] - path(f)[1]))
assert proc.wait() == 0
assert done and seen == list(range(N)), seen
print(f"{N} frames, error median {np.median(errors):.2f} px, max {max(errors):.2f}")
assert np.median(errors) < 1.5 and max(errors) < 4.0
print("ok")
