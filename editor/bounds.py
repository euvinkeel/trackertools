"""Subject bounds sent with a tracking run: a rough box (cx, cy, w, h in source
pixels) per frame, NaN where undefined. See editor/PLAN.md §6.3."""

import base64
import math
from typing import Dict, Optional, Tuple

import numpy as np

DRIFT_MARGIN = 0.1  # same as the frontend (bounds.js): of the box size, on every side


class BoundsTrack:
    def __init__(self, f0: int, data: np.ndarray, guide: bool = True):
        self.f0 = f0
        self.data = data  # (n, 4) float32
        self.guide = guide

    @classmethod
    def from_message(cls, obj: dict) -> "BoundsTrack":
        raw = np.frombuffer(base64.b64decode(obj["data"]), np.float32)
        return cls(int(obj["f0"]), raw[: raw.size // 4 * 4].reshape(-1, 4), bool(obj.get("guide", True)))

    def at(self, f: int) -> Optional[Tuple[float, float, float, float]]:
        i = f - self.f0
        if i < 0 or i >= len(self.data):
            return None
        cx, cy, w, h = (float(v) for v in self.data[i])
        if not math.isfinite(cx) or w <= 0 or h <= 0:
            return None
        return cx, cy, w, h

    def region(self, f: int, pad_x: float, pad_y: float, width: int, height: int):
        """Integer search region (x0, y0, x1, y1): the box plus the drift margin
        plus (pad_x, pad_y), clamped to the frame. None when undefined."""
        b = self.at(f)
        if b is None:
            return None
        cx, cy, w, h = b
        hw = w * (0.5 + DRIFT_MARGIN) + pad_x
        hh = h * (0.5 + DRIFT_MARGIN) + pad_y
        x0 = max(0, int(math.floor(cx - hw)))
        y0 = max(0, int(math.floor(cy - hh)))
        x1 = min(width, int(math.ceil(cx + hw)))
        y1 = min(height, int(math.ceil(cy + hh)))
        return x0, y0, max(x0, x1), max(y0, y1)


def parse_bounds(obj: Optional[dict]) -> Dict[int, BoundsTrack]:
    out = {}
    for sid, b in (obj or {}).items():
        try:
            out[int(sid)] = BoundsTrack.from_message(b)
        except (KeyError, ValueError, TypeError):
            continue
    return out
