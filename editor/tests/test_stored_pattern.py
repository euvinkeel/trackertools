"""Stored-pattern (library) support in templates.py: pixels travel with the
look, valid pixels need no video decode, invalid pixels are an error.

Run: .venv\\Scripts\\python.exe editor\\tests\\test_stored_pattern.py
"""
import base64
import sys
from pathlib import Path

import cv2
import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from templates import build_look, decode_tmpl, unpack  # noqa: E402

W, H = 24, 20
pattern = np.zeros((H, W), np.uint8)
pattern[4:16, 4:20] = 200
pattern[9, 12] = 255  # a distinctive bright dot
ok, png = cv2.imencode(".png", pattern)
assert ok
TMPL = base64.b64encode(png.tobytes()).decode()
SPEC = {"id": 7, "rev": 1, "f": 0, "x": 100, "y": 100, "w": W, "h": H, "hx": 12.0, "hy": 10.0, "mask": ""}


def no_video():
    raise AssertionError("stored pixels must not decode the video")


look = build_look(0, {**SPEC, "tmpl": TMPL}, no_video)
assert look.tmpl.shape == (H, W)
assert np.array_equal(look.tmpl, pattern), "stored pixels are used as-is"
assert look.index == 0 and look.hx == 12.0 and look.hy == 10.0

# Without stored pixels the video crop is used.
frame = np.arange(400, dtype=np.uint8).reshape(20, 20)
look2 = build_look(3, {**SPEC, "w": 20, "h": 20, "x": 0, "y": 0}, lambda: frame)
assert np.array_equal(look2.tmpl, frame)
assert look2.index == 3

# Invalid or mismatched pixels are an error, not a silent video fallback.
for bad in ("bm90IGEgcG5n", base64.b64encode(np.zeros((5, 5), np.uint8).tobytes()).decode()):
    try:
        build_look(0, {**SPEC, "tmpl": bad}, no_video)
    except ValueError as exc:
        assert "invalid" in str(exc)
    else:
        raise AssertionError("invalid stored pixels must raise")

assert decode_tmpl("garbage") is None
assert decode_tmpl(TMPL).shape == (H, W)

# A masked stored look: mask decodes, full/empty collapse to None.
mask = np.ones((H, W), np.uint8) * 255
mask[0, :] = 0
look3 = build_look(1, {**SPEC, "tmpl": TMPL, "mask": base64.b64encode(mask.tobytes()).decode()}, no_video)
assert look3.mask is not None and look3.mask.shape == (H, W)
assert abs(unpack(1 * 4 + 0.8 + 1)[1] - 0.8) < 1e-6

print("Stored patterns: PASS")
