"""CoTracker3 point tracking as a trackertools v2 tracker `method` (DESIGN §6.2).

A worker process: the tracker's job (Rust, tt_track) decodes the video,
resamples every frame through the tracker's view into a fixed-size RGB crop
that follows its guide, and streams the crops here in the order to track them
(a backward job sends them reversed: reversal lives in the frame source, the
model only ever runs forward). This runs v1's online engine (editor/engine.py:
the CoTracker3 online model driven with a rolling window, CUDA graphs on a GPU)
over them and sends back each point on every frame.

Protocol, little-endian, over stdin / stdout:
- stdin, first: one JSON line
  `{"width": 512, "height": 384, "queries": [[f, x, y], ...]}`:
  the crop size, and the points to track, each from stream frame `f` (0 =
  the first frame sent) at crop pixel `(x, y)` (continuous: (0, 0) is the
  top-left corner of the top-left pixel).
- stdin, then per frame: `F` + width × height × 3 bytes (RGB, row-major);
  `E` when there are no more.
- stdout, JSON lines: `{"ready": {"device": ..}}` once the model is loaded;
  `{"f": i, "points": [[x, y, p] | null, ...]}` for each stream frame, in
  order, as soon as it is final (one entry per query, null before its frame;
  `p` is visibility × confidence, 0–1); `{"done": true}` at the end;
  `{"error": ".."}` if it fails.

Arguments: `--weights PATH` (CoTracker3 `scaled_online.pth`; default: the
`TT_COTRACKER_WEIGHTS` environment variable, else torch hub's cache, where v1
downloaded it) and `--device cpu|cuda` (default: cuda when available).
The weights are Meta's, CC-BY-NC 4.0: not part of this repository.
"""

import argparse
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.dirname(HERE))

import numpy as np  # noqa: E402
import torch  # noqa: E402

import engine as v1  # noqa: E402  (v1's network pieces: encode, sample_feats, run_window)
from cotracker.models.build_cotracker import build_cotracker  # noqa: E402


def default_weights() -> str:
    path = os.environ.get("TT_COTRACKER_WEIGHTS")
    if path:
        return path
    return os.path.join(torch.hub.get_dir(), "checkpoints", "scaled_online.pth")


class Engine(v1.TrackingEngine):
    """v1's engine without its job queue: the model, its rolling-window
    pieces and (on a GPU) its CUDA graphs, loaded from a local checkpoint."""

    def __init__(self, weights: str, device: str):
        self.device = device
        if device == "cuda":
            torch.backends.cuda.matmul.allow_tf32 = True
            torch.backends.cudnn.allow_tf32 = True
            torch.backends.cudnn.benchmark = True
        model = build_cotracker(checkpoint=None, offline=False, window_len=16)
        state = torch.load(weights, map_location="cpu")
        model.load_state_dict(state.get("model", state) if isinstance(state, dict) else state)
        self.model = model.to(device).eval()
        self.S = self.model.window_len
        self.step = self.S // 2
        self.overlap = self.S - self.step
        self.stride = self.model.stride
        self.levels = self.model.corr_levels
        self.radius = self.model.corr_radius
        self.iters = 6
        self.fp16_encoder = False
        self.graphed = v1.GraphedWindow(self) if device == "cuda" else None


def send(msg: dict):
    sys.stdout.write(json.dumps(msg, allow_nan=False) + "\n")
    sys.stdout.flush()


def read_frames(stdin, n: int, w: int, h: int):
    """Up to `n` frames; fewer means the stream ended."""
    out = []
    size = w * h * 3
    while len(out) < n:
        tag = stdin.read(1)
        if tag != b"F":
            return out, True
        data = stdin.read(size)
        if len(data) != size:
            raise EOFError("a frame was cut short")
        out.append(np.frombuffer(data, np.uint8).reshape(h, w, 3))
    return out, False


def run(eng: Engine, stdin, header: dict):
    w, h = int(header["width"]), int(header["height"])
    if (w, h) != (v1.MODEL_W, v1.MODEL_H):
        raise ValueError(f"crops must be {v1.MODEL_W}×{v1.MODEL_H}, not {w}×{h}")
    S, step, overlap = eng.S, eng.step, eng.overlap
    # Tracks: the queries (crop px → the model's pixel indices), and a grid of
    # support points for joint context, as in v1.
    queries = [(int(q[0]), float(q[1]) - 0.5, float(q[2]) - 0.5) for q in header["queries"]]
    tracks = [v1.Track(key=str(i), q=q, x=x, y=y, end=1 << 40) for i, (q, x, y) in enumerate(queries)]
    pending = sorted(tracks, key=lambda t: t.q)
    active = []
    emitted = 0
    support_q = None

    def emit(f: int, coords, probs, local: int, live):
        points = []
        for t in tracks:
            j = next((k for k, a in enumerate(live) if a is t), None)
            if j is None or f < t.q:
                points.append(None)
            elif f == t.q:
                points.append([t.x + 0.5, t.y + 0.5, 1.0])
            else:
                x, y, p = float(coords[local, j, 0]), float(coords[local, j, 1]), float(probs[local, j])
                points.append([x + 0.5, y + 0.5, p] if math.isfinite(x) and math.isfinite(y) else None)
        send({"f": f, "points": points})

    ind, first, cache, tail = 0, True, None, None
    while True:
        need = S if first else step
        new, ended = read_frames(stdin, need, w, h)
        n_new = len(new)
        if n_new == 0:
            # The stream ended at the previous window's overlap: its tail is final.
            if tail is not None:
                live, coords, probs = tail
                for k in range(overlap):
                    if ind + k >= emitted:
                        emit(ind + k, coords, probs, k, live)
                        emitted = ind + k + 1
            break
        valid = n_new if first else overlap + n_new
        pyr_new = eng.encode(new)
        pyramid = pyr_new if first else [torch.cat([c, n], dim=1) for c, n in zip(cache, pyr_new)]
        T = pyramid[0].shape[1]
        if T < S:
            pyramid = [torch.cat([p, p[:, -1:].expand(-1, S - T, -1, -1, -1)], dim=1) for p in pyramid]
        if support_q is None or ind + step - support_q >= v1.SUPPORT_REFRESH_FRAMES:
            qs = ind if first else ind + step
            if qs < ind + valid:
                active = [t for t in active if not t.support]
                grid = v1.get_points_on_a_grid(v1.SUPPORT_GRID_SIZE, (v1.MODEL_H, v1.MODEL_W))[0]
                support = [v1.Track(key=f"s{qs}_{i}", q=qs, x=float(p[0]), y=float(p[1]), end=1 << 40, support=True) for i, p in enumerate(grid)]
                eng.sample_feats(pyramid, support, ind)
                active.extend(support)
                support_q = qs
        joining = []
        while pending and pending[0].q < ind + valid:
            joining.append(pending.pop(0))
        if joining:
            eng.sample_feats(pyramid, joining, ind)
            active.extend(joining)
        last = ended or n_new < need
        n_emit = valid if last else min(step, valid)
        coords, vis, conf = eng.run_window(pyramid, active, ind)
        probs = torch.sigmoid(vis) * torch.sigmoid(conf)
        coords_cpu, probs_cpu = coords.float().cpu().numpy(), probs.float().cpu().numpy()
        for k in range(n_emit):
            if ind + k >= emitted:
                emit(ind + k, coords_cpu, probs_cpu, k, active)
                emitted = ind + k + 1
        tail = (list(active), coords_cpu[step:], probs_cpu[step:])
        for j, t in enumerate(active):
            t.prev_coords = coords[step:, j] / eng.stride
            t.prev_vis = vis[step:, j]
            t.prev_conf = conf[step:, j]
        if last:
            break
        cache = [p[:, step:S] for p in pyramid]
        ind += step
        first = False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--weights", default=None)
    ap.add_argument("--device", default=None)
    args = ap.parse_args()
    try:
        weights = args.weights or default_weights()
        if not os.path.exists(weights):
            raise FileNotFoundError(f"CoTracker3 weights not found at {weights} (set TT_COTRACKER_WEIGHTS)")
        device = args.device or ("cuda" if torch.cuda.is_available() else "cpu")
        torch.set_grad_enabled(False)
        eng = Engine(weights, device)
        send({"ready": {"device": device, "window": eng.S}})
        stdin = sys.stdin.buffer
        header = json.loads(stdin.readline())
        with torch.inference_mode():
            run(eng, stdin, header)
        send({"done": True})
    except Exception as exc:  # the job shows it
        import traceback

        traceback.print_exc(file=sys.stderr)
        send({"error": f"{type(exc).__name__}: {exc}"})
        sys.exit(1)


if __name__ == "__main__":
    main()
