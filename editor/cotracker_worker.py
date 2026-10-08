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
once, for all). The streams' windows run in rounds, one window each round, so
they all move on together; a stream that fails says so (`error` with its
`s`) and the others go on. stdin closing ends the worker.

**Batched** (shared mode; `TT_COTRACKER_BATCH=0` turns it off): the streams
with a window ready in the same round run as one batch: one call of the CNN
encoder for all their new frames (it works frame by frame: instance norm, no
batch statistics), and, off CUDA graphs, one pass of the transformer with
every stream a batch row (point counts padded, the padding masked out of the
space attention). Every stream's state (its tracks, the rolling cache) stays
its own; results are the same as one stream at a time within float noise
(editor/tests/test_shared_batch.py). For rounds to find several streams
ready, the input is read ahead on a thread ([`Inbox`]): the jobs keep
sending while a round runs. A batch that fails is run again stream by
stream, so one stream's failure stays its own.

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
And for finding out where the time goes: `TT_COTRACKER_PROFILE=1` logs each
window (each round, batched) to stderr: the time in the encoder, in sampling
the joining tracks' features, in the transformer, and in all.
"""

import argparse
import json
import math
import os
import queue
import sys
import threading
import time
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

    def encode_many(self, groups):
        """[`encode`] of several streams' new frames (a list of frame lists) as
        one batch, split back into one pyramid each. In chunks of up to
        ENCODE_BATCH × ENCODE_CHUNK frames: a few fixed sizes, each tuned once
        by cuDNN; the largest that pads the least (a padded frame costs as
        much as a real one)."""
        frames = [f for g in groups for f in g]
        n = len(frames)
        sizes = [k * v1.ENCODE_CHUNK for k in range(ENCODE_BATCH, 0, -1)]
        chunk = min(sizes, key=lambda c: -(-n // c) * c)
        pyramid = self.encode(frames, chunk)
        out, at = [], 0
        for g in groups:
            out.append([p[:, at:at + len(g)] for p in pyramid])
            at += len(g)
        return out

    def run_windows(self, jobs):
        """[`run_window`] of several streams' windows (`(pyramid, tracks, ind)`
        each) as one pass of the transformer, a batch row each: point counts
        padded to the largest and the padding masked out of the space
        attention (as the CUDA graphs pad theirs), so no row sees another's
        points. On CUDA graphs (captured for one row) they run one by one."""
        if len(jobs) == 1 or self.graphed is not None:
            return [self.run_window(*job) for job in jobs]
        ns = [len(tracks) for _, tracks, _ in jobs]
        n_pad, mask = max(ns), None
        if min(ns) < n_pad:
            # The cross attention tells its mask's orientation by its width:
            # it must not equal the virtual tracks' count (GraphedWindow.bucket).
            n_pad += n_pad == self.model.updateformer.num_virtual_tracks
            mask = torch.zeros(len(jobs), self.S, n_pad, dtype=torch.bool, device=self.device)
            for b, n in enumerate(ns):
                mask[b, :, :n] = True
            mask = mask.view(-1, n_pad)  # (B S, N), as the space attention's tokens

        def pad(x, dim, value):
            extra = n_pad - x.shape[dim]
            if not extra:
                return x
            shape = list(x.shape)
            shape[dim] = extra
            return torch.cat([x, x.new_full(shape, value)], dim)

        ins = [self.window_inputs(tracks, ind) for _, tracks, ind in jobs]
        coords = torch.cat([pad(i[0], 2, 1.0) for i in ins])
        vis = torch.cat([pad(i[1], 2, 0.0) for i in ins])
        conf = torch.cat([pad(i[2], 2, 0.0) for i in ins])
        support = [torch.cat([pad(i[3][lvl], 3, 0.0) for i in ins]) for lvl in range(self.levels)]
        pyramid = [torch.cat(level) for level in zip(*(job[0] for job in jobs))]
        c, v, k = v1.forward_window_safe(self.model, pyramid, coords, support, vis, conf, self.iters, mask)
        return [(c[b, :, :n], v[b, :, :n], k[b, :, :n]) for b, n in enumerate(ns)]


# Batching (module docs): on unless TT_COTRACKER_BATCH=0; the encoder's
# largest batch, in chunks of v1.ENCODE_CHUNK frames.
BATCH = os.environ.get("TT_COTRACKER_BATCH") != "0"
ENCODE_BATCH = 4
# The encoder batched too: off unless TT_COTRACKER_BATCH_ENCODE=1. Measured, it
# never paid: no faster on a PC's CPU, 11% slower on a Mac's MPS and twice
# as slow on a Mac's CPU (2026-10-08), while the transformer's batch saves
# 6–31%. So each stream's frames are encoded as before (chunks of 8).
BATCH_ENCODE = os.environ.get("TT_COTRACKER_BATCH_ENCODE") == "1"

PROFILE = os.environ.get("TT_COTRACKER_PROFILE") == "1"
PROFILE_LOG = []  # what TT_COTRACKER_PROFILE logs, kept (editor/bench_shared.py reads it)


class Clock:
    """Where a window's (a round's) time goes, for `TT_COTRACKER_PROFILE=1`:
    waits for the graphics card at each lap (so only when profiling: it
    stalls the queue). A no-op otherwise."""

    def __init__(self, eng: Engine, what: str, **info):
        self.eng, self.what, self.info, self.parts = eng, what, info, {}
        if PROFILE:
            self.start = self.t = self.now()

    def now(self) -> float:
        if self.eng.device == "cuda":
            torch.cuda.synchronize()
        elif self.eng.device == "mps":
            torch.mps.synchronize()
        return time.perf_counter()

    def lap(self, part: str):
        if PROFILE:
            t = self.now()
            self.parts[part] = self.parts.get(part, 0.0) + t - self.t
            self.t = t

    def done(self, **info):
        if not PROFILE:
            return
        entry = {"what": self.what, **self.info, **info, **self.parts, "total": self.now() - self.start}
        PROFILE_LOG.append(entry)
        times = ", ".join(f"{k} {entry[k] * 1000:.1f} ms" for k in ("encode", "sample", "run", "total") if k in entry)
        about = " ".join(f"{k}={v}" for k, v in {**self.info, **info}.items())
        print(f"profile: {self.what} {about}: {times}", file=sys.stderr, flush=True)


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
        # The window under way (between [`begin`] and [`finish`]).
        self.n_new, self.ended, self.pyramid = 0, False, None

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
        frames come after them. Sets `over` when the stream is finished.
        (A batched round runs the same steps, [`run_round`].)"""
        clock = Clock(self.eng, "window", frames=len(new), tracks=len(self.active))
        if self.begin(new, ended):
            pyr_new = self.eng.encode(new)
            clock.lap("encode")
            self.place(pyr_new)
            clock.lap("sample")
            out = self.eng.run_window(self.pyramid, self.active, self.ind)
            clock.lap("run")
            self.finish(*out)
        clock.done()

    def begin(self, new, ended: bool) -> bool:
        """A window's first step: takes its `new` frames; false when there is
        nothing to run (the stream ended at the previous window's overlap:
        its tail is final, and emitted)."""
        if not new:
            if self.tail is not None:
                live, coords, probs = self.tail
                for k in range(self.eng.overlap):
                    if self.ind + k >= self.emitted:
                        self.emit(self.ind + k, coords, probs, k, live)
                        self.emitted = self.ind + k + 1
            self.over = True
            return False
        self.n_new, self.ended = len(new), ended
        return True

    def place(self, pyr_new):
        """The window's pyramid: the new frames' features (`pyr_new`, from
        the encoder) after the cached overlap; the features of the support
        points and queries that join in it."""
        eng = self.eng
        S, step, overlap = eng.S, eng.step, eng.overlap
        ind, first = self.ind, self.first
        valid = self.n_new if first else overlap + self.n_new
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
        self.pyramid = pyramid

    def finish(self, coords, vis, conf):
        """A window's last step, with the transformer's answer for
        `self.active` (from [`Engine.run_window`]): emits its final frames,
        carries the tracks on, caches the overlap's features."""
        eng = self.eng
        S, step, overlap = eng.S, eng.step, eng.overlap
        ind, pyramid = self.ind, self.pyramid
        valid = self.n_new if self.first else overlap + self.n_new
        last = self.ended or self.n_new < self.need()
        n_emit = valid if last else min(step, valid)
        self.pyramid = None
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


def read_message(stdin):
    """One message of the shared protocol, `(tag, stream, payload)` (the
    header line for `O`, the frame for `F`), or None at the end of the input.
    Every stream's crops are the model's size (a stream with another fails
    when it opens, and its frames are read and dropped)."""
    tag = stdin.read(1)
    if not tag:
        return None
    sid = int.from_bytes(read_exact(stdin, 4), "little")
    if tag == b"O":
        return tag, sid, stdin.readline()
    if tag == b"F":
        data = read_exact(stdin, v1.MODEL_W * v1.MODEL_H * 3)
        return tag, sid, np.frombuffer(data, np.uint8).reshape(v1.MODEL_H, v1.MODEL_W, 3)
    if tag in (b"E", b"X"):
        return tag, sid, None
    raise ValueError(f"unknown message {tag!r}")


# Frames of one stream the read-ahead holds at most: two first windows.
READ_AHEAD = 32


class Inbox:
    """The shared worker's input, read ahead on a thread: the jobs keep
    sending while a round of windows runs, so the next round finds every
    stream whose frames came meanwhile and runs them as one batch (read
    message by message, each stream's window ran as soon as its own frames
    were in: one at a time). It holds at most READ_AHEAD frames of a stream,
    then waits (the jobs wait on the pipe, as before): a stream holding that
    many has a window ready, so the worker always frees some."""

    def __init__(self, stdin):
        self.messages = queue.Queue()
        self.held = {}  # stream -> frames read and not yet taken by a window
        self.room = threading.Condition()
        threading.Thread(target=self._read, args=(stdin,), daemon=True, name="inbox").start()

    def _read(self, stdin):
        try:
            while True:
                with self.room:
                    self.room.wait_for(lambda: max(self.held.values(), default=0) < READ_AHEAD)
                msg = read_message(stdin)
                if msg is not None and msg[0] == b"F":
                    with self.room:
                        self.held[msg[1]] = self.held.get(msg[1], 0) + 1
                self.messages.put(msg)
                if msg is None:
                    return
        except Exception as exc:  # the worker fails with it, as reading in turn did
            self.messages.put(exc)

    def get(self, wait: bool = True) -> list:
        """The messages come so far (`wait`: for the first)."""
        out = []
        if wait:
            out.append(self.messages.get())
        while True:
            try:
                out.append(self.messages.get_nowait())
            except queue.Empty:
                return out

    def free(self, sid: int, n: int):
        """`n` of the stream's frames are taken (or dropped)."""
        with self.room:
            left = self.held.get(sid, 0) - n
            if left > 0:
                self.held[sid] = left
            else:
                self.held.pop(sid, None)
            self.room.notify()


def run_round(eng: Engine, ready) -> list:
    """One window of each ready stream (`(sid, stream, frames, ended)`) as a
    batch (module docs): their encodes as one call, their transformer passes
    as one, every other step each stream's own. Returns each stream's
    failure, or None. A batched step that fails runs again stream by stream
    (it changes no stream's state), so a failure stays the stream's own."""
    import traceback

    errors = [None] * len(ready)
    clock = Clock(eng, "round", streams=len(ready), frames=sum(len(r[2]) for r in ready))

    def each(idx, fn):
        for i in idx:
            try:
                fn(i)
            except Exception as exc:
                errors[i] = exc
        return [i for i in idx if errors[i] is None]

    def batched(what, idx, together, alone):
        try:
            return dict(zip(idx, together(idx))) if idx else {}
        except Exception:
            traceback.print_exc(file=sys.stderr)
            print(f"the batched {what} failed; running it stream by stream", file=sys.stderr)
            out = {}
            each(idx, lambda i: out.__setitem__(i, alone(i)))
            return out

    began = []
    each(range(len(ready)), lambda i: ready[i][1].begin(ready[i][2], ready[i][3]) and began.append(i))
    if BATCH_ENCODE:
        pyramids = batched("encode", began, lambda idx: eng.encode_many([ready[i][2] for i in idx]), lambda i: eng.encode(ready[i][2]))
    else:
        pyramids = {}
        each(began, lambda i: pyramids.__setitem__(i, eng.encode(ready[i][2])))
    clock.lap("encode")
    placed = each(list(pyramids), lambda i: ready[i][1].place(pyramids[i]))
    clock.lap("sample")
    jobs = {i: (ready[i][1].pyramid, ready[i][1].active, ready[i][1].ind) for i in placed}
    outs = batched("window", placed, lambda idx: eng.run_windows([jobs[i] for i in idx]), lambda i: eng.run_window(*jobs[i]))
    clock.lap("run")
    each(list(outs), lambda i: ready[i][1].finish(*outs[i]))
    clock.done(tracks=sum(len(jobs[i][1]) for i in placed))
    return errors


def run_shared(eng: Engine, stdin, batch: Optional[bool] = None):
    """Many streams, one model (the shared protocol, module docs): take the
    messages come so far; then, while any stream has a window's worth of
    frames (or its last ones), run a round: one window of each such stream,
    as one batch ([`run_round`]), or (`batch` off: `TT_COTRACKER_BATCH=0`)
    in turn, reading message by message."""
    import traceback

    batch = BATCH if batch is None else batch
    inbox = Inbox(stdin) if batch else None
    streams = {}  # id -> [Stream, frames waiting, ended]

    def sender(sid):
        return lambda msg: send({"s": sid, **msg})

    def free(sid, n):
        if inbox is not None and n:
            inbox.free(sid, n)

    def drop(sid):
        entry = streams.pop(sid, None)
        if entry:
            free(sid, len(entry[1]))

    def fail(sid, exc):
        traceback.print_exception(type(exc), exc, exc.__traceback__, file=sys.stderr)
        send({"s": sid, "error": f"{type(exc).__name__}: {exc}"})
        drop(sid)

    def alone(r):
        try:
            r[1].window(r[2], r[3])
        except Exception as exc:
            return exc
        return None

    def take(msgs):
        """Applies the messages; the input's end (true, or the exception
        reading it raised) if they reach it, else False."""
        for msg in msgs:
            if msg is None or isinstance(msg, Exception):
                return msg or True
            tag, sid, data = msg
            if tag == b"O":
                try:
                    streams[sid] = [Stream(eng, json.loads(data), sender(sid)), [], False]
                except Exception as exc:  # this stream fails, the others go on
                    fail(sid, exc)
            elif tag == b"F":
                if sid in streams:
                    streams[sid][1].append(data)
                else:  # (a frame for a stream that failed is dropped)
                    free(sid, 1)
            elif tag == b"E":
                if sid in streams:
                    streams[sid][2] = True
            elif tag == b"X":
                drop(sid)
        return False

    while True:
        end = take(inbox.get() if inbox is not None else [read_message(stdin)])
        # Rounds of the windows that can run now, one per stream a round, until none can.
        while True:
            ready = []
            for sid in list(streams):
                stream, frames, ended = streams[sid]
                need = stream.need()
                if len(frames) < need and not ended:
                    continue
                new, streams[sid][1] = frames[:need], frames[need:]
                free(sid, len(new))
                ready.append((sid, stream, new, ended and len(frames) <= need))
            if not ready:
                break
            # (In turn, each stream's window runs as the loop gets to it.)
            outcomes = zip(ready, run_round(eng, ready)) if batch and len(ready) > 1 else ((r, alone(r)) for r in ready)
            for (sid, stream, _, _), exc in outcomes:
                if exc is not None:
                    fail(sid, exc)
                elif stream.over:
                    send({"s": sid, "done": True})
                    drop(sid)
            # What came while the round ran: the next round's.
            if inbox is not None and not end:
                end = take(inbox.get(wait=False))
        if isinstance(end, Exception):
            raise end
        if end:
            return


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
    # And the batched encoder's other chunk sizes (each tuned on its first run).
    if BATCH and BATCH_ENCODE and eng.device != "cpu":
        for k in range(2, ENCODE_BATCH + 1):
            eng.encode([blank] * (k * v1.ENCODE_CHUNK), k * v1.ENCODE_CHUNK)


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
