"""Template tracking restricted to subject bounds, on the synthetic cursor clip.

Run: .venv\\Scripts\\python.exe editor\\tests\\test_template_bounds.py
"""
import base64
import json
import os
import sys
import time
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bounds import BoundsTrack, parse_bounds  # noqa: E402
from frames import FrameServer  # noqa: E402
from templates import TemplateJob, TemplateSegment, unpack  # noqa: E402

TMP = Path(os.environ.get("LOCALAPPDATA", "")) / "Temp" / "opencode"
CLIP = TMP / "cursor_clip.mp4"
TRUTH = json.loads((TMP / "cursor_truth.json").read_text())
META = {"id": "test", "path": str(CLIP), "fps": 60.0, "t0": 0.0, "width": 1920, "height": 1080, "frameCount": 600}
hidden = set(range(*TRUTH["hidden"]))
raw = TRUTH["tips"]
known = [f for f, v in enumerate(raw) if v is not None]
tips = np.stack([np.interp(np.arange(600), known, [raw[f][i] for f in known]) for i in (0, 1)], 1) + 0.5
LOOK = {"id": 1, "rev": 1, "f": 0, "x": 418, "y": 312, "w": 20, "h": 26, "hx": 2.5, "hy": 2.5, "mask": ""}


def run(bounds=None):
    out = {}
    seg = TemplateSegment(key="k", q=0, x=tips[0][0], y=tips[0][1], end=None, threshold=0.7, looks=[LOOK], subject_id=1)

    def emit(m):
        if m["type"] == "results":
            for it in m["items"].values():
                d = it["data"]
                for i in range(len(d) // 3):
                    out[it["f0"] + i] = d[3 * i: 3 * i + 3]

    t0 = time.perf_counter()
    job = TemplateJob(META, 0, [seg], FrameServer(), emit, bounds).start()
    job.done.wait(120)
    return out, time.perf_counter() - t0


def boxes(offset=(0, 0), size=90):
    data = np.zeros((600, 4), np.float32)
    data[:, 0] = tips[:, 0] + 6 + offset[0]
    data[:, 1] = tips[:, 1] + 9 + offset[1]
    data[:, 2:] = size
    return {"1": {"f0": 0, "data": base64.b64encode(data.tobytes()).decode(), "guide": True}}


def summary(out):
    errs, found_hidden, found = [], 0, 0
    for f, (x, y, v) in out.items():
        _, score = unpack(v)
        ok = score >= 0.7
        if f in hidden:
            found_hidden += ok
        elif ok:
            found += 1
            errs.append(np.hypot(x - tips[f][0], y - tips[f][1]))
    return found, found_hidden, float(np.median(errs)) if errs else None, float(np.max(errs)) if errs else None


base, t_base = run()
inb, t_in = run(parse_bounds(boxes()))
wrong, _ = run(parse_bounds(boxes(offset=(500, 300))))
print("no bounds   found %d, found while hidden %d, median %.3f px, max %.2f px, %.2f s" % (*summary(base), t_base))
print("true bounds found %d, found while hidden %d, median %.3f px, max %.2f px, %.2f s" % (*summary(inb), t_in))
fw = summary(wrong)
print("wrong bounds found %d (should be ~0)" % fw[0])
visible = 600 - len(hidden)
assert summary(inb)[0] == visible and summary(inb)[1] == 0 and summary(inb)[2] < 0.2
assert fw[0] <= 1
# Every scored candidate stays inside the (wrong) bounds region. (Score -1 means
# nothing could be searched, e.g. the box is clipped smaller than the template;
# the tracker then reports its last known position.)
b = BoundsTrack.from_message(boxes(offset=(500, 300))["1"])
for f, (x, y, v) in wrong.items():
    if f == 0 or unpack(v)[1] <= -1:
        continue
    x0, y0, x1, y1 = b.region(f, 20, 26, 1920, 1080)
    assert x0 <= x <= x1 and y0 <= y <= y1, (f, x, y, (x0, y0, x1, y1))
print("Template bounds: PASS")
