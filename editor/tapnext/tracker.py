"""TAPNext++ online, one frame at a time, from the app's crops.

- **Input:** each crop (512 × 384 RGB) is *squashed* to the model's square
  input (256², or 512² for the 512 checkpoint), not letterboxed: TAPNext was
  trained and evaluated on videos resized to a square whatever their aspect
  (TAP-Vid, Kubric, PointOdyssey), so a uniform-scale letterbox would show
  it bars it never saw and spend a quarter of its patches on them. Squashing
  is linear per axis, so the mapping back is exact: crop (x, y) ↔ model
  (x · 256 / W, y · 256 / H), both continuous ((0, 0) the top-left corner
  of the top-left pixel). 256 is the model's *coordinate* space (its query
  embedding and its output bins) at either input resolution.
- **Score:** p = sigmoid(visibility logit): TAPNext has no separate
  confidence.
- **Queries on later frames** (reset points): one recurrent state serves all
  of a stream's queries. A query is fed as an "unknown" token before its
  frame, its point token on its frame, then "mask" tokens; the model is
  causal in time, so this online run equals the offline run over the whole
  clip (tests/test_tapnext.py checks it): no second state, no re-run.
- **Support points:** each query brings a grid of helper points around it
  (`support_points`), tracked with it, never sent.
- **State:** fixed size per stream, whatever its length: per layer an RG-LRU
  state and a 3-frame conv cache for every token, (patches + queries) × 768
  floats × 4 × 12 layers (about 150 MB at 256², 600 MB at 512²).
"""

from __future__ import annotations

import math
from typing import Callable, Optional

import numpy as np
import torch
from torch.nn import functional as F

from .tapnext_torch import TAPNext

# The model's coordinate space (its query embedding and output bins), and
# its native input; the 512 checkpoint takes 512² frames, same coordinates.
MODEL_SIZE = 256
# Recomputed when the model is built (a fixed sin-cos table, 200 MB as fp32):
# trimmed weights leave it out.
DERIVED = {"query_pos_embed"}
# Helper points per query (header `support`) on a grid this far around it
# (model pixels: ±16 × ±12 crop px), at most SUPPORT_TOTAL in a stream (each
# is a token beside a 256² frame's 1024 patches: 1024 more doubled the time
# on the CPU, 256 cost 15–30%). DeepMind's VOTS
# 2026 entry used 64 within 16; on the sprite fixture a tighter grid, on the
# target itself, kept every cold start on it (64 within 16 lost one of four).
SUPPORT = 64
SUPPORT_RADIUS = 8.0
SUPPORT_TOTAL = 256


def read_weights(path: str):
    """The TAPNext state dict and the input resolution it was trained at,
    from DeepMind's Lightning checkpoint or our trimmed one (trim_checkpoint.py).
    Loaded `weights_only` (tensors and plain containers, no pickled code)
    and memory-mapped (DeepMind's optimizer state, 1.5 GB, is never read)."""
    ck = torch.load(path, map_location="cpu", weights_only=True, mmap=True)
    if isinstance(ck, dict) and "tapnext" in ck:  # trimmed
        return ck["tapnext"], int(ck.get("input_resolution", MODEL_SIZE))
    sd = ck.get("state_dict", ck) if isinstance(ck, dict) else ck
    sd = {k.removeprefix("tapnext."): v for k, v in sd.items() if torch.is_tensor(v)}
    return sd, guess_resolution(path, ck)


def guess_resolution(path: str, ck) -> int:
    """The input size: the 512 checkpoint's training config says it
    (`input_resolution`), the 256 one's doesn't; else the file name."""
    try:
        res = ck["cfg"]["common_params"].get("input_resolution")
        if res:
            return int(res[0] if isinstance(res, (list, tuple)) else res)
    except (KeyError, TypeError, AttributeError):
        pass
    return 512 if "512" in str(path).replace("\\", "/").rsplit("/", 1)[-1] else MODEL_SIZE


class Model:
    """TAPNext++ on a device, fp32 weights (a trimmed fp16 file is widened:
    the CPU has no fast fp16 matmul); on CUDA it runs under fp16 autocast,
    as DeepMind's own wrapper does."""

    def __init__(self, weights: str, device: str):
        sd, self.resolution = read_weights(weights)
        net = TAPNext(image_size=(MODEL_SIZE, MODEL_SIZE))
        missing, unexpected = net.load_state_dict(sd, strict=False)
        if set(missing) - DERIVED or unexpected:
            raise ValueError(f"not TAPNext weights: missing {sorted(set(missing) - DERIVED)[:5]}, unexpected {unexpected[:5]}")
        self.net = net.float().to(device).eval()
        self.device = device
        self.autocast = device == "cuda"

    def prepare(self, frame: np.ndarray) -> torch.Tensor:
        """A crop as the model's input: [1, 1, R, R, 3] in [-1, 1], squashed
        with an antialiased bilinear filter (on the CPU: it is cheap there,
        and antialiasing isn't on every GPU backend)."""
        t = torch.from_numpy(np.array(frame)).permute(2, 0, 1)[None].float()
        r = self.resolution
        if t.shape[-2:] != (r, r):
            t = F.interpolate(t, size=(r, r), mode="bilinear", align_corners=False, antialias=True)
        t = t / 127.5 - 1.0
        return t.permute(0, 2, 3, 1)[None].to(self.device)

    def step(self, frame: np.ndarray, queries: Optional[torch.Tensor], state):
        """One frame: (positions [Q, 2] as model (x, y), visibility logits
        [Q], the new state). `queries` ([1, Q, 3] as (t, y, x)) on the first
        frame only; the state keeps them."""
        video = self.prepare(frame)
        ctx = torch.autocast("cuda", dtype=torch.float16) if self.autocast else torch.autocast("cpu", enabled=False)
        with ctx:
            tracks, _, vis, state = self.net(video=video, query_points=queries, state=state)
        # Tracks come as (y, x), the queries' order.
        return tracks[0, 0].float().flip(-1), vis[0, 0, :, 0].float(), state


def support_points(x: float, y: float, n: int):
    """`n` helper points on a grid around model point (x, y), within
    SUPPORT_RADIUS and the frame, from its query's frame. Tracked jointly and
    never sent, they keep a lone point on its target from a cold start: on
    the sprite fixture one query alone was lost (200 px) from two starts of
    four, with these from none."""
    if n <= 0:
        return []
    side = math.ceil(math.sqrt(n))
    g = [(i + 0.5) / side * 2 * SUPPORT_RADIUS - SUPPORT_RADIUS for i in range(side)]
    pts = [(x + dx, y + dy) for dy in g for dx in g][:n]
    hi = MODEL_SIZE - 1e-3
    return [(min(max(px, 0.0), hi), min(max(py, 0.0), hi)) for px, py in pts]


class Stream:
    """One tracker's stream through TAPNext++: the worker's `Stream`
    interface (`need`, `window`, `over`), a frame at a time, each frame's
    points sent as soon as it is computed (no window to wait for)."""

    def __init__(self, model: Model, header: dict, send: Callable[[dict], None]):
        w, h = int(header["width"]), int(header["height"])
        self.model, self.w, self.h, self.send = model, w, h, send
        self.queries = [(int(q[0]), float(q[1]), float(q[2])) for q in header["queries"]]
        sx, sy = MODEL_SIZE / w, MODEL_SIZE / h
        q = [[f, y * sy, x * sx] for f, x, y in self.queries]
        n = min(int(header.get("support", SUPPORT)), SUPPORT_TOTAL // max(len(self.queries), 1))
        q += [[f, y, x] for f, mx, my in [(f, x * sx, y * sy) for f, x, y in self.queries] for x, y in support_points(mx, my, n)]
        self.query_tensor = torch.tensor(q, dtype=torch.float32, device=model.device)[None]
        self.scale = torch.tensor([w / MODEL_SIZE, h / MODEL_SIZE], device=model.device)
        self.state = None
        self.f = 0
        self.over = False

    def need(self) -> int:
        return 1

    def window(self, new, ended: bool):
        for frame in new:
            self.track(frame)
        if ended:
            self.over = True

    def track(self, frame: np.ndarray):
        f = self.f
        points = [None] * len(self.queries)
        if self.queries:
            first = self.state is None
            xy, vis, self.state = self.model.step(frame, self.query_tensor if first else None, self.state)
            xy = (xy * self.scale).cpu().numpy()
            p = torch.sigmoid(vis).cpu().numpy()
            for k, (qf, qx, qy) in enumerate(self.queries):
                if f == qf:
                    points[k] = [qx, qy, 1.0]  # the user's point, exactly
                elif f > qf:
                    x, y = float(xy[k, 0]), float(xy[k, 1])
                    points[k] = [x, y, float(p[k])] if math.isfinite(x) and math.isfinite(y) else None
        self.send({"f": f, "points": points})
        self.f += 1
