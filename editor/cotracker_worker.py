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

**Shared** (`--shared`): one worker, one model on the graphics card, for
several trackers at once (the app's CoTracker jobs all go through one: each
its own *stream*, numbered by the app). Every message on stdin starts with
a tag and the stream's number (u32):
- `O` + id + the JSON header line above: a stream opens;
- `F` + id + a frame; `E` + id: its frames end; `X` + id: drop it (its job
  was cancelled: nothing more is sent about it).
On stdout the same messages as above, each with `"s": id` (but `ready`,
once, for all). The streams' windows run in turn, one window each round, so
they all move on together; a stream that fails says so (`error` with its
`s`) and the others go on. stdin closing ends the worker.

Arguments: `--weights PATH` (CoTracker3 `scaled_online.pth`; default: the
`TT_COTRACKER_WEIGHTS` environment variable, else torch hub's cache, where v1
downloaded it) and `--device cpu|cuda|mps` (default: CUDA when available,
else Apple Silicon's GPU (MPS) if a trial window runs there, else the CPU;
`TT_COTRACKER_DEVICE` sets it too).
The weights are Meta's, CC-BY-NC 4.0: not part of this repository.

Switches in the environment (the app's own, inherited), for finding out
what makes a graphics card reset:
- `TT_COTRACKER_GRAPHS=0`: no CUDA graphs (each window runs its kernels one
  by one, slower).
- `TT_COTRACKER_BENCHMARK=0`: cuDNN does not try out its algorithms on the
  first window (`torch.backends.cudnn.benchmark` off).
"""

import argparse
import json
import math
import os
import sys
from typing import Optional

# Ops MPS lacks run on the CPU instead of failing (must be set before torch loads).
os.environ.setdefault("PYTORCH_ENABLE_MPS_FALLBACK", "1")

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
        graphs = os.environ.get("TT_COTRACKER_GRAPHS") != "0"
        benchmark = os.environ.get("TT_COTRACKER_BENCHMARK") != "0"
        if device == "cuda":
            torch.backends.cuda.matmul.allow_tf32 = True
            torch.backends.cudnn.allow_tf32 = True
            torch.backends.cudnn.benchmark = benchmark
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
        self.graphed = v1.GraphedWindow(self) if device == "cuda" and graphs else None
        if not (graphs and benchmark):
            print(f"CUDA graphs {'on' if graphs else 'off'}, cuDNN benchmark {'on' if benchmark else 'off'}", file=sys.stderr)


def pick_engine(weights: str, asked: Optional[str]) -> "Engine":
    """The engine on the asked device, else the best one that works: CUDA,
    then MPS (after a trial window: not every op of the model is proven
    there), then the CPU."""
    if asked:
        return Engine(weights, asked)
    if torch.cuda.is_available():
        return Engine(weights, "cuda")
    if getattr(torch.backends, "mps", None) is not None and torch.backends.mps.is_available():
        try:
            eng = Engine(weights, "mps")
            with torch.inference_mode():
                frames = [np.zeros((v1.MODEL_H, v1.MODEL_W, 3), np.uint8)] * eng.S
                pyramid = eng.encode(frames)
                probe = [v1.Track(key="probe", q=0, x=100.0, y=100.0, end=1)]
                eng.sample_feats(pyramid, probe, 0)
                eng.run_window(pyramid, probe, 0)
            return eng
        except Exception as exc:  # fall back, and say why on stderr
            print(f"MPS failed ({type(exc).__name__}: {exc}); using the CPU", file=sys.stderr)
    return Engine(weights, "cpu")


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


class Stream:
    """One tracker's stream through the model: its queries, the support
    points, the rolling window's cache, and what it has emitted. Fed its
    frames a window at a time ([`Stream.window`]); `send` takes each result
    message (with the stream's number added in shared mode)."""

    def __init__(self, eng: Engine, header: dict, send):
        w, h = int(header["width"]), int(header["height"])
        if (w, h) != (v1.MODEL_W, v1.MODEL_H):
            raise ValueError(f"crops must be {v1.MODEL_W}×{v1.MODEL_H}, not {w}×{h}")
        self.eng, self.w, self.h, self.send = eng, w, h, send
        # Tracks: the queries (crop px → the model's pixel indices), and a grid of
        # support points for joint context, as in v1.
        queries = [(int(q[0]), float(q[1]) - 0.5, float(q[2]) - 0.5) for q in header["queries"]]
        self.tracks = [v1.Track(key=str(i), q=q, x=x, y=y, end=1 << 40) for i, (q, x, y) in enumerate(queries)]
        self.pending = sorted(self.tracks, key=lambda t: t.q)
        self.active = []
        self.emitted = 0
        self.support_q = None
        self.ind, self.first, self.cache, self.tail = 0, True, None, None
        self.over = False

    def need(self) -> int:
        """Frames its next window takes."""
        return self.eng.S if self.first else self.eng.step

    def emit(self, f: int, coords, probs, local: int, live):
        points = []
        for t in self.tracks:
            j = next((k for k, a in enumerate(live) if a is t), None)
            if j is None or f < t.q:
                points.append(None)
            elif f == t.q:
                points.append([t.x + 0.5, t.y + 0.5, 1.0])
            else:
                x, y, p = float(coords[local, j, 0]), float(coords[local, j, 1]), float(probs[local, j])
                points.append([x + 0.5, y + 0.5, p] if math.isfinite(x) and math.isfinite(y) else None)
        self.send({"f": f, "points": points})

    def window(self, new, ended: bool):
        """Track one window with `new` frames (up to [`need`]); `ended`: no
        frames come after them. Sets `over` when the stream is finished."""
        eng = self.eng
        S, step, overlap = eng.S, eng.step, eng.overlap
        need, n_new = self.need(), len(new)
        if n_new == 0:
            # The stream ended at the previous window's overlap: its tail is final.
            if self.tail is not None:
                live, coords, probs = self.tail
                for k in range(overlap):
                    if self.ind + k >= self.emitted:
                        self.emit(self.ind + k, coords, probs, k, live)
                        self.emitted = self.ind + k + 1
            self.over = True
            return
        ind, first = self.ind, self.first
        valid = n_new if first else overlap + n_new
        pyr_new = eng.encode(new)
        pyramid = pyr_new if first else [torch.cat([c, n], dim=1) for c, n in zip(self.cache, pyr_new)]
        T = pyramid[0].shape[1]
        if T < S:
            pyramid = [torch.cat([p, p[:, -1:].expand(-1, S - T, -1, -1, -1)], dim=1) for p in pyramid]
        if self.support_q is None or ind + step - self.support_q >= v1.SUPPORT_REFRESH_FRAMES:
            qs = ind if first else ind + step
            if qs < ind + valid:
                self.active = [t for t in self.active if not t.support]
                grid = v1.get_points_on_a_grid(v1.SUPPORT_GRID_SIZE, (v1.MODEL_H, v1.MODEL_W))[0]
                support = [v1.Track(key=f"s{qs}_{i}", q=qs, x=float(p[0]), y=float(p[1]), end=1 << 40, support=True) for i, p in enumerate(grid)]
                eng.sample_feats(pyramid, support, ind)
                self.active.extend(support)
                self.support_q = qs
        joining = []
        while self.pending and self.pending[0].q < ind + valid:
            joining.append(self.pending.pop(0))
        if joining:
            eng.sample_feats(pyramid, joining, ind)
            self.active.extend(joining)
        last = ended or n_new < need
        n_emit = valid if last else min(step, valid)
        coords, vis, conf = eng.run_window(pyramid, self.active, ind)
        probs = torch.sigmoid(vis) * torch.sigmoid(conf)
        coords_cpu, probs_cpu = coords.float().cpu().numpy(), probs.float().cpu().numpy()
        for k in range(n_emit):
            if ind + k >= self.emitted:
                self.emit(ind + k, coords_cpu, probs_cpu, k, self.active)
                self.emitted = ind + k + 1
        self.tail = (list(self.active), coords_cpu[step:], probs_cpu[step:])
        for j, t in enumerate(self.active):
            t.prev_coords = coords[step:, j] / eng.stride
            t.prev_vis = vis[step:, j]
            t.prev_conf = conf[step:, j]
        if last:
            self.over = True
            return
        self.cache = [p[:, step:S] for p in pyramid]
        self.ind += step
        self.first = False


def run(eng: Engine, stdin, header: dict):
    """One stream, the whole worker's (the plain protocol)."""
    stream = Stream(eng, header, send)
    while not stream.over:
        new, ended = read_frames(stdin, stream.need(), stream.w, stream.h)
        stream.window(new, ended)


def read_exact(stdin, n: int) -> bytes:
    data = stdin.read(n)
    if len(data) != n:
        raise EOFError("the input was cut short")
    return data


def run_shared(eng: Engine, stdin):
    """Many streams, one model (the shared protocol, module docs): read a
    message; then, while any stream has a window's worth of frames (or its
    last ones), run one window of each such stream in turn."""
    streams = {}  # id -> [Stream, frames waiting, ended]

    def sender(sid):
        return lambda msg: send({"s": sid, **msg})

    def fail(sid, exc):
        import traceback

        traceback.print_exc(file=sys.stderr)
        send({"s": sid, "error": f"{type(exc).__name__}: {exc}"})
        streams.pop(sid, None)

    while True:
        tag = stdin.read(1)
        if not tag:
            return
        sid = int.from_bytes(read_exact(stdin, 4), "little")
        if tag == b"O":
            header = json.loads(stdin.readline())
            try:
                streams[sid] = [Stream(eng, header, sender(sid)), [], False]
            except Exception as exc:  # this stream fails, the others go on
                fail(sid, exc)
        elif tag == b"F":
            entry = streams.get(sid)
            # (A frame for a stream that failed is read and dropped.)
            size = entry[0].w * entry[0].h * 3 if entry else v1.MODEL_W * v1.MODEL_H * 3
            data = read_exact(stdin, size)
            if entry:
                entry[1].append(np.frombuffer(data, np.uint8).reshape(entry[0].h, entry[0].w, 3))
        elif tag == b"E":
            if sid in streams:
                streams[sid][2] = True
        elif tag == b"X":
            streams.pop(sid, None)
        else:
            raise ValueError(f"unknown message {tag!r}")
        # Windows that can run now, one per stream a round, until none can.
        progress = True
        while progress:
            progress = False
            for sid in list(streams):
                stream, frames, ended = streams[sid]
                need = stream.need()
                if len(frames) < need and not ended:
                    continue
                take, streams[sid][1] = frames[:need], frames[need:]
                try:
                    stream.window(take, ended and len(frames) <= need)
                except Exception as exc:
                    fail(sid, exc)
                    continue
                progress = True
                if stream.over:
                    send({"s": sid, "done": True})
                    streams.pop(sid, None)


def practice(eng: Engine):
    """One practice stream on blank frames, results dropped, before the
    shared worker says it is ready: the first windows on a card are slow
    (cuDNN picks its algorithms, the CUDA graphs are captured), so the
    first real tracker doesn't wait for that. `TT_COTRACKER_WARM=0`: none."""
    if os.environ.get("TT_COTRACKER_WARM") == "0":
        return
    s = Stream(eng, {"width": v1.MODEL_W, "height": v1.MODEL_H, "queries": [[0, v1.MODEL_W / 2, v1.MODEL_H / 2]]}, lambda msg: None)
    blank = np.zeros((v1.MODEL_H, v1.MODEL_W, 3), np.uint8)
    s.window([blank] * s.need(), False)
    s.window([blank] * s.need(), True)
    if not s.over:
        s.window([], True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--weights", default=None)
    ap.add_argument("--device", default=None)
    ap.add_argument("--shared", action="store_true", help="many streams, one model (module docs)")
    args = ap.parse_args()
    try:
        weights = args.weights or default_weights()
        if not os.path.exists(weights):
            raise FileNotFoundError(f"CoTracker3 weights not found at {weights} (set TT_COTRACKER_WEIGHTS)")
        torch.set_grad_enabled(False)
        eng = pick_engine(weights, args.device or os.environ.get("TT_COTRACKER_DEVICE"))
        if args.shared:
            with torch.inference_mode():
                practice(eng)
        send({"ready": {"device": eng.device, "window": eng.S}})
        stdin = sys.stdin.buffer
        with torch.inference_mode():
            if args.shared:
                run_shared(eng, stdin)
            else:
                header = json.loads(stdin.readline())
                run(eng, stdin, header)
                send({"done": True})
    except Exception as exc:  # the job (every stream, shared) shows it
        import traceback

        traceback.print_exc(file=sys.stderr)
        send({"error": f"{type(exc).__name__}: {exc}"})
        sys.exit(1)


if __name__ == "__main__":
    main()
