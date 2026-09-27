"""Streaming CoTracker3 engine for arbitrarily long videos.

The stock online predictor keeps every past prediction and re-pads it on each
step, so memory and time grow with video length. The model itself only needs
the last `window_len - step` frames of history, so this module drives the same
network with a rolling window. Tracks can join (at their query frame) and leave
(at their end frame) at any time during a run.
"""

import base64
import math
import os
import queue
import threading
import time
from dataclasses import dataclass, field
from typing import Callable, Dict, List, Optional, Tuple

import av
import cv2
import numpy as np
import torch
import torch.nn.functional as F

from cotracker.models.core.cotracker import cotracker3_online
from cotracker.models.core.model_utils import get_points_on_a_grid
from frames import FrameReader

MODEL_H, MODEL_W = 384, 512
VIS_THRESHOLD = 0.6
SUPPORT_GRID_SIZE = 6
SUPPORT_REFRESH_FRAMES = 96
PREVIEW_WIDTH = 480
PREVIEW_INTERVAL_S = 0.12
FLUSH_INTERVAL_S = 0.1
ENCODE_CHUNK = 8
SCALE_THREADS = max(1, min(8, (os.cpu_count() or 4) // 2))

# ---- CUDA-graph friendly replacements ------------------------------------
# The model builds a few small constant tensors from Python lists on every
# call, which is illegal during CUDA graph capture. These cached versions are
# numerically identical.

_CONSTS: Dict[tuple, torch.Tensor] = {}


def _const(values: tuple, device) -> torch.Tensor:
    key = (values, str(device))
    t = _CONSTS.get(key)
    if t is None:
        t = torch.tensor(values, device=device, dtype=torch.float32)
        _CONSTS[key] = t
    return t


def _bilinear_sampler_safe(input, coords, align_corners=True, padding_mode="border"):
    sizes = input.shape[2:]
    if len(sizes) == 3:
        coords = torch.stack([coords[..., 1], coords[..., 2], coords[..., 0]], dim=-1)
    if align_corners:
        scale = _const(tuple(2 / max(s - 1, 1) for s in reversed(sizes)), coords.device)
    else:
        scale = _const(tuple(2 / s for s in reversed(sizes)), coords.device)
    coords = coords * scale - 1
    return F.grid_sample(input, coords, align_corners=align_corners, padding_mode=padding_mode)


cotracker3_online.bilinear_sampler = _bilinear_sampler_safe


def _posenc_safe(x, min_deg, max_deg):
    scales = _const(tuple(float(2**i) for i in range(min_deg, max_deg)), x.device).to(x.dtype)
    xb = (x[..., None, :] * scales[:, None]).reshape(list(x.shape[:-1]) + [-1])
    four_feat = torch.sin(torch.cat([xb, xb + 0.5 * torch.pi], dim=-1))
    return torch.cat([x] + [four_feat], dim=-1)


def forward_window_safe(model, fmaps_pyramid, coords, track_feat_support_pyramid,
                        vis, conf, iters, mask=None):
    """Copy of CoTrackerThreeOnline.forward_window returning only the final
    iteration, with cached constants and an optional track mask (used to pad
    the track count for CUDA graphs without affecting real tracks)."""
    B, S = fmaps_pyramid[0].shape[:2]
    N = coords.shape[2]
    r = 2 * model.corr_radius + 1
    scale = _const(
        (model.model_resolution[1] / model.stride, model.model_resolution[0] / model.stride),
        coords.device,
    )
    for _ in range(iters):
        coords = coords.detach()
        coords_init = coords.view(B * S, N, 2)
        corr_embs = []
        for i in range(model.corr_levels):
            corr_feat = model.get_correlation_feat(fmaps_pyramid[i], coords_init / 2**i)
            track_feat_support = (
                track_feat_support_pyramid[i]
                .view(B, 1, r, r, N, model.latent_dim)
                .squeeze(1)
                .permute(0, 3, 1, 2, 4)
            )
            corr_volume = torch.einsum("btnhwc,bnijc->btnhwij", corr_feat, track_feat_support)
            corr_embs.append(model.corr_mlp(corr_volume.reshape(B * S * N, r * r * r * r)))
        corr_embs = torch.cat(corr_embs, dim=-1).view(B, S, N, -1)

        rel_fwd = F.pad(coords[:, :-1] - coords[:, 1:], (0, 0, 0, 0, 0, 1)) / scale
        rel_bwd = F.pad(coords[:, 1:] - coords[:, :-1], (0, 0, 0, 0, 1, 0)) / scale
        rel_pos = _posenc_safe(torch.cat([rel_fwd, rel_bwd], dim=-1), 0, 10)

        x = torch.cat([vis, conf, corr_embs, rel_pos], dim=-1)
        x = x.permute(0, 2, 1, 3).reshape(B * N, S, -1)
        x = x + model.interpolate_time_embed(x, S)
        x = x.view(B, N, S, -1)
        delta = model.updateformer(x, mask=mask, add_space_attn=True)

        vis = vis + delta[..., 2:3].permute(0, 2, 1, 3)
        conf = conf + delta[..., 3:].permute(0, 2, 1, 3)
        coords = coords + delta[..., :2].permute(0, 2, 1, 3)
    return coords[..., :2] * float(model.stride), vis[..., 0], conf[..., 0]


class GraphedWindow:
    """Replays forward_window_safe from captured CUDA graphs, one per padded
    track count. Removes the ~80 ms of Python/kernel-launch overhead per step."""

    def __init__(self, engine: "TrackingEngine"):
        self.e = engine
        self.graphs: Dict[int, dict] = {}
        self.pool = None
        self.pyr: Optional[List[torch.Tensor]] = None

    @staticmethod
    def bucket(n: int) -> int:
        b = max(8, -(-n // 8) * 8)
        # The cross-attention block infers mask orientation from its width; 64
        # equals the number of virtual tracks, so skip that size.
        return 72 if b == 64 else b

    def _capture(self, n_pad: int, pyramid_like: List[torch.Tensor]) -> dict:
        e, dev, S = self.e, self.e.device, self.e.S
        if self.pyr is None:
            self.pyr = [torch.zeros_like(p) for p in pyramid_like]
        C = e.model.latent_dim
        r2 = (2 * e.radius + 1) ** 2
        st = {
            "coords": torch.ones(1, S, n_pad, 2, device=dev),
            "vis": torch.zeros(1, S, n_pad, 1, device=dev),
            "conf": torch.zeros(1, S, n_pad, 1, device=dev),
            "support": [torch.zeros(1, 1, r2, n_pad, C, device=dev) for _ in range(e.levels)],
            "mask": torch.ones(S, n_pad, dtype=torch.bool, device=dev),
        }

        def fn():
            return forward_window_safe(e.model, self.pyr, st["coords"], st["support"],
                                       st["vis"], st["conf"], e.iters, st["mask"])

        side = torch.cuda.Stream()
        side.wait_stream(torch.cuda.current_stream())
        with torch.cuda.stream(side):
            for _ in range(2):
                fn()
        torch.cuda.current_stream().wait_stream(side)
        graph = torch.cuda.CUDAGraph()
        with torch.cuda.graph(graph, pool=self.pool):
            st["out"] = fn()
        self.pool = graph.pool()
        st["graph"] = graph
        self.graphs[n_pad] = st
        return st

    def run(self, pyramid, coords, vis, conf, support, n: int):
        n_pad = self.bucket(n)
        st = self.graphs.get(n_pad) or self._capture(n_pad, pyramid)
        for dst, src in zip(self.pyr, pyramid):
            dst.copy_(src)
        st["coords"][:, :, :n].copy_(coords)
        st["coords"][:, :, n:].fill_(1.0)
        st["vis"][:, :, :n].copy_(vis)
        st["vis"][:, :, n:].zero_()
        st["conf"][:, :, :n].copy_(conf)
        st["conf"][:, :, n:].zero_()
        for dst, src in zip(st["support"], support):
            dst[..., :n, :].copy_(src)
            dst[..., n:, :].zero_()
        st["mask"][:, :n] = True
        st["mask"][:, n:] = False
        st["graph"].replay()
        c, v, k = st["out"]
        return c[0, :, :n].clone(), v[0, :, :n].clone(), k[0, :, :n].clone()


def estimate_gop(path: str, max_packets: int = 2000) -> int:
    """Median keyframe interval (in frames) over the start of the video."""
    with av.open(path) as container:
        stream = container.streams.video[0]
        last, gaps, i = None, [], 0
        for pkt in container.demux(stream):
            if pkt.size == 0:
                continue
            if pkt.is_keyframe:
                if last is not None:
                    gaps.append(i - last)
                    if len(gaps) >= 8:
                        break
                last = i
            i += 1
            if i >= max_packets:
                break
        if gaps:
            return int(sorted(gaps)[len(gaps) // 2])
        return max(i, 1)


def probe_video(path: str) -> dict:
    with av.open(path) as container:
        stream = container.streams.video[0]
        guessed = float(stream.guessed_rate) if stream.guessed_rate else 0.0
        average = float(stream.average_rate) if stream.average_rate else 0.0
        if guessed and average and abs(guessed - average) / guessed > 0.01:
            fps, rate = average, stream.average_rate
        else:
            fps = guessed or average or 30.0
            rate = stream.guessed_rate or stream.average_rate

        duration = None
        if stream.duration and stream.time_base:
            duration = float(stream.duration * stream.time_base)
        elif container.duration:
            duration = container.duration / av.time_base

        first_time = 0.0
        for frame in container.decode(stream):
            first_time = float(frame.time or 0.0)
            break

        frame_count = stream.frames or 0
        if not frame_count and duration:
            frame_count = int(math.floor(duration * fps + 1e-6))
        if duration is None:
            duration = frame_count / fps

        return {
            "width": stream.codec_context.width,
            "height": stream.codec_context.height,
            "fps": fps,
            "fpsRational": f"{rate.numerator}/{rate.denominator}" if rate else f"{fps:.6f}",
            "frameCount": int(frame_count),
            "duration": duration,
            "codec": stream.codec_context.name,
            "t0": first_time,
            "gop": estimate_gop(path),
        }


def to_model_rgb(frame) -> np.ndarray:
    return frame.to_ndarray(width=MODEL_W, height=MODEL_H, format="rgb24",
                            interpolation="AREA", threads=SCALE_THREADS)


@dataclass
class Track:
    key: str
    q: int  # absolute query frame
    x: float  # query position, model-resolution pixels
    y: float
    end: int  # exclusive
    support: bool = False
    feats: Optional[List[torch.Tensor]] = None  # per level: (49, C)
    prev_coords: Optional[torch.Tensor] = None  # (overlap, 2) stride units
    prev_vis: Optional[torch.Tensor] = None  # (overlap,) logits
    prev_conf: Optional[torch.Tensor] = None
    # Bounds starting guess: frame -> subject box (cx, cy, w, h) in model pixels
    # or None, and the CPU copy of the last window's final position (model px).
    guide: Optional[Callable[[int], Optional[tuple]]] = None
    last: Optional[Tuple[float, float]] = None


@dataclass
class Segment:
    key: str
    q: int
    x: float  # source pixels (continuous, 0..width)
    y: float
    end: Optional[int] = None
    subject_id: Optional[int] = None


class TrackingEngine:
    def __init__(self, repo_root: str, device: Optional[str] = None):
        self.device = device or ("cuda" if torch.cuda.is_available() else "cpu")
        if self.device == "cuda":
            torch.backends.cuda.matmul.allow_tf32 = True
            torch.backends.cudnn.allow_tf32 = True
            torch.backends.cudnn.benchmark = True
        predictor = torch.hub.load(repo_root, "cotracker3_online", source="local")
        self.model = predictor.model.to(self.device).eval()
        self.S = self.model.window_len
        self.step = self.S // 2
        self.overlap = self.S - self.step
        self.stride = self.model.stride
        self.levels = self.model.corr_levels
        self.radius = self.model.corr_radius
        self.iters = 6
        self.fp16_encoder = False
        self.graphed = GraphedWindow(self) if self.device == "cuda" else None
        # All GPU work runs on one long-lived thread: CUDA/cuDNN set up
        # per-thread state that costs ~1 s, and it serializes jobs for free.
        self._tasks: "queue.Queue" = queue.Queue()
        self._worker = threading.Thread(target=self._work, daemon=True)
        self._worker.start()
        self.ready = threading.Event()
        self._tasks.put(self.warmup)
        self._tasks.put(self.ready.set)

    def _work(self):
        while True:
            task = self._tasks.get()
            try:
                task()
            except Exception:
                import traceback

                traceback.print_exc()

    def warmup(self):
        """Pay cudnn autotuning and graph capture for common sizes up front."""
        frames = [np.zeros((MODEL_H, MODEL_W, 3), np.uint8)] * self.S
        with torch.inference_mode():
            pyramid = self.encode(frames)
            for n in (SUPPORT_GRID_SIZE**2 + 1, SUPPORT_GRID_SIZE**2 + 9):
                tracks = [Track(key=str(i), q=0, x=10.0 + i, y=10.0, end=1) for i in range(n)]
                self.sample_feats(pyramid, tracks, 0)
                self.run_window(pyramid, tracks)
        if self.device == "cuda":
            torch.cuda.synchronize()

    # ---- network pieces -------------------------------------------------

    def encode(self, frames: List[np.ndarray]) -> List[torch.Tensor]:
        # Always run the encoder on fixed-size chunks so cudnn autotunes once.
        chunks = []
        for i in range(0, len(frames), ENCODE_CHUNK):
            part = frames[i:i + ENCODE_CHUNK]
            n = len(part)
            if n < ENCODE_CHUNK:
                part = part + [part[-1]] * (ENCODE_CHUNK - n)
            x = torch.from_numpy(np.stack(part)).to(self.device, non_blocking=True)
            x = x.permute(0, 3, 1, 2).float()
            x = 2 * (x / 255.0) - 1.0
            if self.fp16_encoder:
                with torch.autocast("cuda", dtype=torch.float16):
                    chunks.append(self.model.fnet(x)[:n].float())
            else:
                chunks.append(self.model.fnet(x)[:n])
        fmaps = torch.cat(chunks) if len(chunks) > 1 else chunks[0]
        norm = torch.sqrt(
            torch.clamp(torch.sum(fmaps * fmaps, dim=1, keepdim=True), min=1e-12)
        )
        fmaps = fmaps / norm
        pyramid = [fmaps]
        for _ in range(self.levels - 1):
            fmaps = F.avg_pool2d(fmaps, 2, stride=2)
            pyramid.append(fmaps)
        return [p.unsqueeze(0) for p in pyramid]  # (1, T, C, h, w)

    def sample_feats(self, pyramid, tracks: List[Track], ind: int):
        frames = torch.tensor([[t.q - ind for t in tracks]], device=self.device)
        coords = torch.tensor(
            [[[t.x / self.stride, t.y / self.stride] for t in tracks]],
            device=self.device,
            dtype=torch.float32,
        )
        per_level = []
        for i in range(self.levels):
            _, support = self.model.get_track_feat(
                pyramid[i], frames, coords / 2**i, support_radius=self.radius
            )
            per_level.append(support[0])  # (49, N, C)
        for j, track in enumerate(tracks):
            track.feats = [lvl[:, j] for lvl in per_level]

    def guided(self, t: Track, frames: range, ref_f: int, ref_xy: Tuple[float, float]):
        """Starting positions for `frames` (stride units): the reference
        position carried along with the subject's box (translation + scale)
        instead of copied. None when there is no box at the reference frame."""
        b0 = t.guide(ref_f)
        if b0 is None or not len(frames):
            return None
        rows = []
        last = ref_xy
        for f in frames:
            b = t.guide(f)
            if b is not None:
                kx = min(4.0, max(0.25, b[2] / b0[2]))
                ky = min(4.0, max(0.25, b[3] / b0[3]))
                last = (b[0] + (ref_xy[0] - b0[0]) * kx, b[1] + (ref_xy[1] - b0[1]) * ky)
            rows.append(last)  # frames without a box hold the previous guess
        return torch.tensor(rows, device=self.device, dtype=torch.float32) / self.stride

    def run_window(self, pyramid, tracks: List[Track], ind: Optional[int] = None):
        S, overlap = self.S, self.overlap
        coords, vis, conf = [], [], []
        for t in tracks:
            guide = t.guide is not None and ind is not None and not t.support
            if t.prev_coords is not None:
                tail = None
                if guide and t.last is not None:
                    tail = self.guided(t, range(ind + overlap, ind + S), ind + overlap - 1, t.last)
                if tail is None:
                    tail = t.prev_coords[-1:].expand(S - overlap, 2)
                c = torch.cat([t.prev_coords, tail])
                v = torch.cat([t.prev_vis, t.prev_vis[-1:].expand(S - overlap)])
                k = torch.cat([t.prev_conf, t.prev_conf[-1:].expand(S - overlap)])
            else:
                c = torch.tensor(
                    [t.x / self.stride, t.y / self.stride], device=self.device
                ).expand(S, 2)
                if guide:
                    k0 = max(ind, t.q + 1)
                    rows = self.guided(t, range(k0, ind + S), t.q, (t.x, t.y))
                    if rows is not None:
                        c = torch.cat([c[: k0 - ind], rows])
                v = torch.zeros(S, device=self.device)
                k = torch.zeros(S, device=self.device)
            coords.append(c)
            vis.append(v)
            conf.append(k)
        coords = torch.stack(coords, dim=1)[None].float()  # (1, S, N, 2)
        vis = torch.stack(vis, dim=1)[None, ..., None].float()
        conf = torch.stack(conf, dim=1)[None, ..., None].float()
        support = [
            torch.stack([t.feats[i] for t in tracks], dim=1)[None, None]
            for i in range(self.levels)
        ]  # (1, 1, 49, N, C)
        if self.graphed is not None:
            try:
                return self.graphed.run(pyramid, coords, vis, conf, support, len(tracks))
            except Exception:
                import traceback

                traceback.print_exc()
                print("CUDA graph path failed; falling back to eager execution.")
                self.graphed = None
        c, v, k = forward_window_safe(self.model, pyramid, coords, support, vis, conf, self.iters)
        return c[0], v[0], k[0]  # (S,N,2) px, (S,N), (S,N)

    # ---- jobs -------------------------------------------------------------

    def start_job(self, video: dict, start_frame: int, segments: List[Segment], emit, bounds=None):
        job = TrackJob(self, video, start_frame, segments, emit, bounds)
        self._tasks.put(job._run)
        return job


class TrackJob:
    def __init__(self, engine: TrackingEngine, video: dict, start_frame: int,
                 segments: List[Segment], emit: Callable[[dict], None], bounds=None):
        self.engine = engine
        self.video = video
        self.start_frame = start_frame
        self.segments = segments
        self.bounds = bounds or {}  # subject id -> BoundsTrack (bounds.py)
        self.emit = emit
        self.stop_event = threading.Event()
        self.done = threading.Event()
        self.retire_map: Dict[str, int] = {}  # segment key -> stop at this frame
        self.frame = start_frame
        self.frame_count = int(video["frameCount"])
        width, height = video["width"], video["height"]
        self.sx = (MODEL_W - 1) / max(width - 1, 1)
        self.sy = (MODEL_H - 1) / max(height - 1, 1)
        self._pending_results: Dict[str, list] = {}
        self._last_flush = 0.0
        self._last_preview = 0.0
        self._last_status = 0.0

    def halt(self):
        self.stop_event.set()

    def retire(self, entries):
        """Stop the given segments at their cutoff frames at the next window
        boundary. Results already emitted stay archived; the client's effective
        cutoff keeps them from contributing."""
        for e in entries or []:
            key = e.get("key")
            frame = e.get("frame")
            if key is None or frame is None:
                continue
            f = int(frame)
            prev = self.retire_map.get(key)
            self.retire_map[key] = f if prev is None else min(prev, f)

    def _apply_retire(self, active: List[Track]):
        for t in active:
            f = self.retire_map.get(t.key)
            if f is not None and f < t.end:
                t.end = f

    # coordinate conversion: source continuous pixels <-> model pixel indices
    def to_model(self, x, y):
        return (x - 0.5) * self.sx, (y - 0.5) * self.sy

    def to_source(self, x, y):
        return x / self.sx + 0.5, y / self.sy + 0.5

    def guide_for(self, subject_id):
        """Subject box in model pixels per frame, if its bounds guide CoTracker."""
        b = self.bounds.get(subject_id)
        if b is None or not b.guide:
            return None

        def guide(f):
            v = b.at(f)
            if v is None:
                return None
            cx, cy, w, h = v
            return (cx - 0.5) * self.sx, (cy - 0.5) * self.sy, w * self.sx, h * self.sy

        return guide

    def _status(self, state, **extra):
        self.emit({"type": "status", "state": state, "frame": self.frame,
                   "startFrame": self.start_frame, **extra})

    def _queue_result(self, key, q, frame, x, y, p):
        buf = self._pending_results.get(key)
        if buf is not None and buf[1] + len(buf[2]) // 3 != frame:
            self._flush_key(key)
            buf = None
        if buf is None:
            buf = [q, frame, []]
            self._pending_results[key] = buf
        buf[2].extend((round(x, 3), round(y, 3), round(p, 4)))

    def _flush_key(self, key):
        buf = self._pending_results.pop(key, None)
        if buf and buf[2]:
            self.emit({"type": "results", "items": {key: {"q": buf[0], "f0": buf[1], "data": buf[2]}}})

    def _flush(self, force=False):
        now = time.monotonic()
        if not force and now - self._last_flush < FLUSH_INTERVAL_S:
            return
        self._last_flush = now
        items = {k: {"q": b[0], "f0": b[1], "data": b[2]}
                 for k, b in self._pending_results.items() if b[2]}
        self._pending_results.clear()
        if items:
            self.emit({"type": "results", "items": items})

    def _preview(self, img, frame, tracks, coords, probs, local):
        now = time.monotonic()
        if now - self._last_preview < PREVIEW_INTERVAL_S:
            return
        self._last_preview = now
        width, height = self.video["width"], self.video["height"]
        ph = max(2, int(round(PREVIEW_WIDTH * height / width)))
        small = cv2.resize(img, (PREVIEW_WIDTH, ph), interpolation=cv2.INTER_AREA)
        ok, jpg = cv2.imencode(".jpg", cv2.cvtColor(small, cv2.COLOR_RGB2BGR),
                               [cv2.IMWRITE_JPEG_QUALITY, 72])
        if not ok:
            return
        points = []
        for j, t in enumerate(tracks):
            if t.support or frame < t.q or frame >= t.end:
                continue
            x, y = self.to_source(float(coords[local, j, 0]), float(coords[local, j, 1]))
            points.append([t.key, round(x, 2), round(y, 2), round(float(probs[local, j]), 3)])
        self.emit({"type": "preview", "frame": frame,
                   "image": "data:image/jpeg;base64," + base64.b64encode(jpg.tobytes()).decode(),
                   "points": points})

    def _support_tracks(self, q: int) -> List[Track]:
        grid = get_points_on_a_grid(SUPPORT_GRID_SIZE, (MODEL_H, MODEL_W))[0]
        return [Track(key=f"__support{q}_{i}", q=q, x=float(p[0]), y=float(p[1]),
                      end=self.frame_count + self.engine.S, support=True)
                for i, p in enumerate(grid)]

    def _run(self):
        try:
            if not self.stop_event.is_set():
                with torch.inference_mode():
                    self._track()
            self._flush(force=True)
            if self.stop_event.is_set():
                self._status("halted")
            else:
                self._status("done")
        except Exception as exc:  # report to client instead of dying silently
            import traceback

            traceback.print_exc()
            self._flush(force=True)
            self._status("error", message=f"{type(exc).__name__}: {exc}")
        finally:
            self.done.set()

    def _track(self):
        eng = self.engine
        S, step, overlap = eng.S, eng.step, eng.overlap
        N_total = self.frame_count

        pending: List[Track] = []
        width, height = self.video["width"], self.video["height"]
        for seg in self.segments:
            end = min(seg.end if seg.end is not None else N_total, N_total)
            if seg.q >= end or seg.q < 0:
                continue
            if not (0 <= seg.x <= width and 0 <= seg.y <= height):
                continue  # the model can't start tracking from off-frame
            mx, my = self.to_model(seg.x, seg.y)
            pending.append(Track(key=seg.key, q=seg.q, x=mx, y=my, end=end, guide=self.guide_for(seg.subject_id)))
        pending.sort(key=lambda t: t.q)
        if not pending:
            return

        ind = pending[0].q
        self.start_frame = ind
        active: List[Track] = []
        reader: Optional[FrameReader] = None
        cache = None  # pyramid for frames [ind, ind+overlap)
        tail = None  # (tracks, coords, probs) predicted for frames [ind, ind+overlap)
        support_q = None
        first = True
        processed = 0
        t_start = time.monotonic()
        self.frame = ind
        self._status("running", totalFrames=N_total)

        try:
            while not self.stop_event.is_set():
                active = [t for t in active if t.end > ind]
                self._apply_retire(active)
                if not any(not t.support for t in active):
                    if not pending:
                        break
                    if reader is None or pending[0].q >= ind + S:
                        # Nothing to track here: jump straight to the next track's start.
                        ind = pending[0].q
                        active, cache, tail, first, support_q = [], None, None, True, None
                        if reader is not None:
                            reader.stop_event.set()
                        reader = FrameReader(self.video["path"], self.video["fps"],
                                             self.video.get("t0", 0.0), ind, threading.Event(),
                                             convert=to_model_rgb)

                need = S if first else step
                new_frames = reader.read(need)
                if self.stop_event.is_set():
                    break
                n_new = len(new_frames)
                valid = min(n_new if first else overlap + n_new, N_total - ind)
                if valid <= 0 or n_new == 0:
                    # Video ended exactly at the previous window's overlap: its tail is final.
                    # Anything still pending starts beyond the end of the video.
                    if tail is not None:
                        t_tracks, t_coords, t_probs = tail
                        self._emit_range(t_tracks, t_coords, t_probs, ind, 0,
                                         min(overlap, N_total - ind), overlap)
                        self.frame = min(ind + overlap, N_total)
                    break

                pyr_new = eng.encode(new_frames)
                if first:
                    pyramid = pyr_new
                else:
                    pyramid = [torch.cat([c, n], dim=1) for c, n in zip(cache, pyr_new)]
                T = pyramid[0].shape[1]
                if T < S:
                    pyramid = [torch.cat([p, p[:, -1:].expand(-1, S - T, -1, -1, -1)], dim=1)
                               for p in pyramid]

                # Support grid for joint tracking context, refreshed periodically.
                if support_q is None or ind + step - support_q >= SUPPORT_REFRESH_FRAMES:
                    qs = ind if first else ind + step
                    if qs < ind + valid:
                        active = [t for t in active if not t.support]
                        new_support = self._support_tracks(qs)
                        eng.sample_feats(pyramid, new_support, ind)
                        active.extend(new_support)
                        support_q = qs

                joining = []
                while pending and pending[0].q < ind + valid:
                    t = pending.pop(0)
                    f = self.retire_map.get(t.key)
                    if f is not None and f <= t.q:
                        continue
                    joining.append(t)
                if joining:
                    eng.sample_feats(pyramid, joining, ind)
                    active.extend(joining)

                last = n_new < need or ind + valid >= N_total
                n_emit = valid if last else min(step, valid)
                if any(not t.support for t in active):
                    coords, vis, conf = eng.run_window(pyramid, active, ind)
                    probs = torch.sigmoid(vis) * torch.sigmoid(conf)
                    coords_cpu = coords.float().cpu().numpy()
                    probs_cpu = probs.float().cpu().numpy()
                    self._emit_range(active, coords_cpu, probs_cpu, ind, 0, n_emit, valid)
                    tail = (list(active), coords_cpu[step:], probs_cpu[step:])
                    local = min((n_new - 1) + (0 if first else overlap), valid - 1)
                    self._preview(new_frames[-1], ind + local, active, coords_cpu, probs_cpu, local)
                    for j, t in enumerate(active):
                        t.prev_coords = coords[step:, j] / eng.stride
                        t.prev_vis = vis[step:, j]
                        t.prev_conf = conf[step:, j]
                        if t.guide is not None:
                            t.last = (float(coords_cpu[S - 1, j, 0]), float(coords_cpu[S - 1, j, 1]))
                else:
                    tail = None
                self.frame = ind + n_emit

                processed += n_new
                now = time.monotonic()
                if now - self._last_status > 0.25:
                    self._last_status = now
                    elapsed = max(now - t_start, 1e-6)
                    self._status("running", fps=round(processed / elapsed, 1),
                                 totalFrames=N_total)
                self._flush()

                if last:
                    break  # anything still pending starts beyond the end of the video

                cache = [p[:, step:S] for p in pyramid]
                ind += step
                first = False
        finally:
            if reader is not None:
                reader.stop_event.set()

    def _emit_range(self, tracks, coords, probs, ind, lo, hi, valid):
        hi = min(hi, valid)
        for j, t in enumerate(tracks):
            if t.support:
                continue
            f_lo = max(ind + lo, t.q)
            f_hi = min(ind + hi, t.end)
            for f in range(f_lo, f_hi):
                local = f - ind
                if f == t.q:
                    x, y, p = t.x, t.y, 1.0
                else:
                    x, y, p = float(coords[local, j, 0]), float(coords[local, j, 1]), float(probs[local, j])
                sx, sy = self.to_source(x, y)
                self._queue_result(t.key, t.q, f, sx, sy, p)
