"""Template trackers: follow a rigid pattern (e.g. a mouse cursor) with OpenCV
template matching on full-resolution grayscale frames.

A tracker has one or more "looks" (template + optional mask + hotspot). Per
frame, every look is matched in a small window around the predicted position;
the best score wins. When nothing scores above the tracker's threshold, a
coarse half-resolution search over the whole frame proposes candidates that
are refined at full resolution. See editor/PLAN.md §7.

Results use the CoTracker message format ([x, y, v] per frame) with
v = look_index * 4 + (score + 1), score in [-1, 1] (TM_CCOEFF_NORMED).
"""

import base64
import math
import threading
import time
from collections import OrderedDict
from dataclasses import dataclass, field
from typing import Callable, Dict, List, Optional, Tuple

import cv2
import numpy as np

from bounds import BoundsTrack
from frames import FrameReader, FrameServer, crop, to_gray

LOCAL_RADIUS = 48
LOCAL_RADIUS_MAX = 400
REFINE_RADIUS = 6
GLOBAL_CANDIDATES = 5
PREVIEW_WIDTH = 480
PREVIEW_INTERVAL_S = 0.12
FLUSH_INTERVAL_S = 0.1


def pack(look: int, score: float) -> float:
    return look * 4 + (max(-1.0, min(1.0, score)) + 1.0)


def unpack(v: float) -> Tuple[int, float]:
    look = int(v // 4)
    return look, v - look * 4 - 1.0


@dataclass
class Look:
    index: int
    hx: float  # hotspot relative to the template's top-left, source pixels
    hy: float
    tmpl: np.ndarray  # (h, w) uint8
    mask: Optional[np.ndarray]  # (h, w) uint8 0/255, None = whole box
    tmpl_half: Optional[np.ndarray] = None

    @property
    def w(self):
        return self.tmpl.shape[1]

    @property
    def h(self):
        return self.tmpl.shape[0]


def decode_mask(b64: Optional[str], w: int, h: int) -> Optional[np.ndarray]:
    if not b64:
        return None
    raw = np.frombuffer(base64.b64decode(b64), np.uint8)
    if raw.size != w * h:
        return None
    m = (raw.reshape(h, w) > 0).astype(np.uint8) * 255
    if m.all() or not m.any():
        return None  # full or empty: match the whole box
    return m


def decode_tmpl(b64: str) -> Optional[np.ndarray]:
    """Grayscale pixels of a saved pattern (base64 PNG), or None."""
    try:
        raw = np.frombuffer(base64.b64decode(b64), np.uint8)
        img = cv2.imdecode(raw, cv2.IMREAD_GRAYSCALE)
    except Exception:
        return None
    return img if img is not None and img.size else None


def build_look(index: int, spec: dict, gray) -> Look:
    x, y, w, h = int(spec["x"]), int(spec["y"]), int(spec["w"]), int(spec["h"])
    tmpl = None
    if spec.get("tmpl"):
        # A stored pattern (library): its pixels travel with the look, so the
        # original video is not needed. Invalid pixels are an error, never a
        # silent fallback to unrelated video content.
        tmpl = decode_tmpl(spec["tmpl"])
        if tmpl is None or tmpl.shape != (h, w):
            raise ValueError(f"pattern pixels are invalid (expected {w}x{h})")
    if tmpl is None:
        frame = gray() if callable(gray) else gray
        tmpl = crop(frame, x, y, w, h).copy()
    mask = decode_mask(spec.get("mask"), w, h)
    look = Look(index=index, hx=float(spec["hx"]), hy=float(spec["hy"]), tmpl=tmpl, mask=mask)
    if w // 2 >= 4 and h // 2 >= 4:
        look.tmpl_half = cv2.resize(tmpl, (w // 2, h // 2), interpolation=cv2.INTER_AREA)
    return look


def _parabola(a: float, b: float, c: float) -> float:
    d = a - 2 * b + c
    if d >= 0 or not math.isfinite(d):
        return 0.0
    return max(-0.5, min(0.5, 0.5 * (a - c) / d))


def _peak(res: np.ndarray) -> Tuple[float, float, float]:
    """(score, x, y) of the maximum with a sub-pixel parabola fit."""
    _, score, _, (mx, my) = cv2.minMaxLoc(res)
    h, w = res.shape
    dx = _parabola(res[my, mx - 1], score, res[my, mx + 1]) if 0 < mx < w - 1 else 0.0
    dy = _parabola(res[my - 1, mx], score, res[my + 1, mx]) if 0 < my < h - 1 else 0.0
    return float(score), mx + dx, my + dy


def _match(image: np.ndarray, tmpl: np.ndarray, mask: Optional[np.ndarray]) -> np.ndarray:
    res = cv2.matchTemplate(image, tmpl, cv2.TM_CCOEFF_NORMED, mask=mask)
    # Flat image patches give 0/0 (NaN/inf); treat them as non-matches.
    return np.nan_to_num(res, nan=-1.0, posinf=-1.0, neginf=-1.0, copy=False)


def match_window(gray: np.ndarray, look: Look, x0: float, y0: float, x1: float, y1: float,
                 region: Optional[Tuple[int, int, int, int]] = None):
    """Best match with the template's top-left inside [x0, x1] x [y0, y1].
    Returns (score, hotspot_x, hotspot_y) or None."""
    H, W = gray.shape
    lx0, ly0, lx1, ly1 = 0, 0, W - look.w, H - look.h
    if region is not None:  # keep the whole template inside the region
        lx0, ly0 = max(lx0, region[0]), max(ly0, region[1])
        lx1, ly1 = min(lx1, region[2] - look.w), min(ly1, region[3] - look.h)
    ix0 = max(lx0, int(math.floor(x0)))
    iy0 = max(ly0, int(math.floor(y0)))
    ix1 = min(lx1, int(math.ceil(x1)))
    iy1 = min(ly1, int(math.ceil(y1)))
    if ix1 < ix0 or iy1 < iy0:
        return None
    patch = gray[iy0:iy1 + look.h, ix0:ix1 + look.w]
    score, mx, my = _peak(_match(patch, look.tmpl, look.mask))
    return score, ix0 + mx + look.hx, iy0 + my + look.hy


def global_search(gray: np.ndarray, half: np.ndarray, look: Look,
                  region: Optional[Tuple[int, int, int, int]] = None):
    """Coarse unmasked search at half resolution, then masked refinement of the
    best candidates at full resolution. Returns (score, x, y) or None."""
    H, W = gray.shape
    rx0, ry0, rx1, ry1 = region if region is not None else (0, 0, W, H)
    candidates = []
    if look.tmpl_half is not None:
        hx0, hy0 = rx0 // 2, ry0 // 2
        sub = half[hy0:ry1 // 2, hx0:rx1 // 2]
        th, tw = look.tmpl_half.shape
        if sub.shape[0] < th or sub.shape[1] < tw:
            return None
        res = _match(sub, look.tmpl_half, None)
        rad = max(2, min(tw, th) // 2)
        for _ in range(GLOBAL_CANDIDATES):
            _, v, _, (mx, my) = cv2.minMaxLoc(res)
            if v <= -1.0:
                break
            candidates.append(((hx0 + mx) * 2, (hy0 + my) * 2))
            res[max(0, my - rad):my + rad + 1, max(0, mx - rad):mx + rad + 1] = -1.0
    else:  # template too small to downscale: full-resolution coarse pass
        sub = gray[ry0:ry1, rx0:rx1]
        if sub.shape[0] < look.h or sub.shape[1] < look.w:
            return None
        res = _match(sub, look.tmpl, None)
        rad = max(2, min(look.w, look.h) // 2)
        for _ in range(GLOBAL_CANDIDATES):
            _, v, _, (mx, my) = cv2.minMaxLoc(res)
            if v <= -1.0:
                break
            candidates.append((rx0 + mx, ry0 + my))
            res[max(0, my - rad):my + rad + 1, max(0, mx - rad):mx + rad + 1] = -1.0
    best = None
    for cx, cy in candidates:
        m = match_window(gray, look, cx - REFINE_RADIUS, cy - REFINE_RADIUS,
                         cx + REFINE_RADIUS, cy + REFINE_RADIUS, region)
        if m is not None and (best is None or m[0] > best[0]):
            best = m
    return best


@dataclass
class TemplateSegment:
    key: str
    q: int
    x: float  # hotspot at the seed frame, source pixels
    y: float
    end: Optional[int]
    threshold: float
    looks: List[dict]
    subject_id: Optional[int] = None


@dataclass
class _Track:
    key: str
    q: int
    x: float
    y: float
    end: int
    threshold: float
    looks: List[Look]
    last: Tuple[float, float] = (0.0, 0.0)
    vel: Tuple[float, float] = (0.0, 0.0)
    found: bool = True
    region: Optional[Tuple[int, int, int, int]] = None
    bounds: Optional[BoundsTrack] = None  # subject bounds: the search stays inside them


def track_frame(t: _Track, gray: np.ndarray, get_half: Callable[[], np.ndarray]):
    """Match one tracker on one frame. Returns (x, y, look_index, score) and
    updates the tracker's motion state."""
    px, py = t.last
    if t.found:
        px, py = px + t.vel[0], py + t.vel[1]
    speed = math.hypot(*t.vel)
    r = min(LOCAL_RADIUS_MAX, LOCAL_RADIUS + 2 * speed)
    best = None  # (score, x, y, look)
    for look in t.looks:
        tlx, tly = px - look.hx, py - look.hy
        m = match_window(gray, look, tlx - r, tly - r, tlx + r, tly + r, t.region)
        if m is not None and (best is None or m[0] > best[0]):
            best = (m[0], m[1], m[2], look.index)
    if best is None or best[0] < t.threshold:
        half = get_half()
        for look in t.looks:
            m = global_search(gray, half, look, t.region)
            if m is not None and (best is None or m[0] > best[0]):
                best = (m[0], m[1], m[2], look.index)
    if best is not None and best[0] >= t.threshold:
        if t.found:
            t.vel = (best[1] - t.last[0], best[2] - t.last[1])
        else:
            t.vel = (0.0, 0.0)
        t.last = (best[1], best[2])
        t.found = True
    else:
        t.vel = (0.0, 0.0)
        t.found = False
    if best is None:
        return t.last[0], t.last[1], 0, -1.0
    return best[1], best[2], best[3], best[0]


class _LookCache:
    """Templates cut from exact frames, keyed by look identity and revision."""

    def __init__(self, limit=256):
        self.limit = limit
        self.items: "OrderedDict[tuple, Look]" = OrderedDict()
        self.lock = threading.Lock()

    def get(self, frames: FrameServer, video: dict, index: int, spec: dict) -> Look:
        key = (video["path"], spec.get("id"), spec.get("rev"), int(spec["f"]),
               int(spec["x"]), int(spec["y"]), int(spec["w"]), int(spec["h"]), spec.get("mask"),
               hash(spec.get("tmpl") or ""))
        with self.lock:
            look = self.items.get(key)
            if look is not None:
                self.items.move_to_end(key)
        if look is None:
            # Lazy frame access: stored-pattern looks don't decode the video.
            base = build_look(index, spec, lambda: frames.get(video, int(spec["f"]), "gray"))
            look = base
            with self.lock:
                self.items[key] = base
                while len(self.items) > self.limit:
                    self.items.popitem(last=False)
        if look.index != index or look.hx != float(spec["hx"]) or look.hy != float(spec["hy"]):
            look = Look(index=index, hx=float(spec["hx"]), hy=float(spec["hy"]),
                        tmpl=look.tmpl, mask=look.mask, tmpl_half=look.tmpl_half)
        return look


LOOKS = _LookCache()


class TemplateJob:
    def __init__(self, video: dict, start_frame: int, segments: List[TemplateSegment],
                 frames: FrameServer, emit: Callable[[dict], None],
                 bounds: Optional[Dict[int, BoundsTrack]] = None):
        self.video = video
        self.bounds = bounds or {}
        self.start_frame = start_frame
        self.segments = segments
        self.frames = frames
        self.emit = emit
        self.stop_event = threading.Event()
        self.done = threading.Event()
        self.retire_map: Dict[str, int] = {}  # segment key -> stop at this frame
        self.frame = start_frame
        self.frame_count = int(video["frameCount"])
        self._pending: Dict[str, list] = {}
        self._last_flush = 0.0
        self._last_preview = 0.0
        self._last_status = 0.0
        self.thread = threading.Thread(target=self._run, daemon=True)

    def start(self):
        self.thread.start()
        return self

    def halt(self):
        self.stop_event.set()

    def retire(self, entries):
        """Stop the given segments at their cutoff frames."""
        for e in entries or []:
            key = e.get("key")
            frame = e.get("frame")
            if key is None or frame is None:
                continue
            f = int(frame)
            prev = self.retire_map.get(key)
            self.retire_map[key] = f if prev is None else min(prev, f)

    def _status(self, state, **extra):
        self.emit({"type": "status", "state": state, "frame": self.frame,
                   "startFrame": self.start_frame, **extra})

    def _queue(self, key, q, f, x, y, v):
        buf = self._pending.get(key)
        if buf is not None and buf[1] + len(buf[2]) // 3 != f:
            self._flush_key(key)
            buf = None
        if buf is None:
            buf = [q, f, []]
            self._pending[key] = buf
        buf[2].extend((round(float(x), 3), round(float(y), 3), round(float(v), 4)))

    def _flush_key(self, key):
        buf = self._pending.pop(key, None)
        if buf and buf[2]:
            self.emit({"type": "results", "items": {key: {"q": buf[0], "f0": buf[1], "data": buf[2]}}})

    def _flush(self, force=False):
        now = time.monotonic()
        if not force and now - self._last_flush < FLUSH_INTERVAL_S:
            return
        self._last_flush = now
        items = {k: {"q": b[0], "f0": b[1], "data": b[2]} for k, b in self._pending.items() if b[2]}
        self._pending.clear()
        if items:
            self.emit({"type": "results", "items": items})

    def _preview(self, gray, f, points):
        now = time.monotonic()
        if now - self._last_preview < PREVIEW_INTERVAL_S:
            return
        self._last_preview = now
        H, W = gray.shape
        ph = max(2, int(round(PREVIEW_WIDTH * H / W)))
        small = cv2.resize(gray, (PREVIEW_WIDTH, ph), interpolation=cv2.INTER_AREA)
        ok, jpg = cv2.imencode(".jpg", small, [cv2.IMWRITE_JPEG_QUALITY, 72])
        if ok:
            self.emit({"type": "preview", "frame": f,
                       "image": "data:image/jpeg;base64," + base64.b64encode(jpg.tobytes()).decode(),
                       "points": points})

    def _run(self):
        try:
            self._track()
            self._flush(force=True)
            self._status("halted" if self.stop_event.is_set() else "done")
        except Exception as exc:
            import traceback

            traceback.print_exc()
            self._flush(force=True)
            self._status("error", message=f"{type(exc).__name__}: {exc}")
        finally:
            self.done.set()

    def _track(self):
        N = self.frame_count
        pending: List[_Track] = []
        W, H = self.video["width"], self.video["height"]
        for seg in self.segments:
            end = min(seg.end if seg.end is not None else N, N)
            if seg.q < 0 or seg.q >= end or not seg.looks:
                continue
            if not (0 <= seg.x <= W and 0 <= seg.y <= H):
                continue
            # The packed value is the look's stable slot (assigned by the
            # project), not its array position: removing a look must not shift
            # the meaning of already-stored results.
            looks = [LOOKS.get(self.frames, self.video, int(spec.get("slot", i)), spec)
                     for i, spec in enumerate(seg.looks)]
            pending.append(_Track(key=seg.key, q=seg.q, x=seg.x, y=seg.y, end=end,
                                  threshold=float(seg.threshold), looks=looks, last=(seg.x, seg.y),
                                  bounds=self.bounds.get(seg.subject_id)))
            if self.stop_event.is_set():
                return
        pending.sort(key=lambda t: t.q)
        if not pending:
            return
        self.start_frame = pending[0].q
        self.frame = self.start_frame
        self._status("running", totalFrames=N)
        active: List[_Track] = []
        reader: Optional[FrameReader] = None
        f = pending[0].q
        processed = 0
        t_start = time.monotonic()
        try:
            while not self.stop_event.is_set():
                active = [t for t in active if t.end > f]
                for t in active:
                    stop = self.retire_map.get(t.key)
                    if stop is not None and stop < t.end:
                        t.end = stop
                if not active:
                    if not pending:
                        break
                    if reader is None or pending[0].q > f + 60:
                        f = pending[0].q
                        if reader is not None:
                            reader.stop_event.set()
                        reader = FrameReader(self.video["path"], self.video["fps"],
                                             self.video.get("t0", 0.0), f, threading.Event(),
                                             convert=to_gray, maxsize=16)
                item = reader.read_indexed(1)
                if not item or self.stop_event.is_set():
                    break
                idx, gray = item[0]
                if idx >= N:
                    break
                f = idx
                while pending and pending[0].q <= f:
                    t = pending.pop(0)
                    stop = self.retire_map.get(t.key)
                    if stop is not None and stop <= t.q:
                        continue
                    if stop is not None:
                        t.end = min(t.end, stop)
                    active.append(t)
                half_cache = []

                def get_half():
                    if not half_cache:
                        half_cache.append(cv2.resize(gray, (gray.shape[1] // 2, gray.shape[0] // 2),
                                                     interpolation=cv2.INTER_AREA))
                    return half_cache[0]

                points = []
                for t in active:
                    if f < t.q or f >= t.end:
                        continue
                    if f == t.q:
                        x, y, li, score = t.x, t.y, 0, 1.0
                    else:
                        if t.bounds is not None:
                            pad_x = max(look.w for look in t.looks)
                            pad_y = max(look.h for look in t.looks)
                            t.region = t.bounds.region(f, pad_x, pad_y, W, H)
                        x, y, li, score = track_frame(t, gray, get_half)
                    self._queue(t.key, t.q, f, x, y, pack(li, score))
                    points.append([t.key, round(float(x), 2), round(float(y), 2), 1.0 if score >= t.threshold else 0.0])
                self._preview(gray, f, points)
                self.frame = f + 1
                processed += 1
                now = time.monotonic()
                if now - self._last_status > 0.25:
                    self._last_status = now
                    self._status("running", fps=round(processed / max(now - t_start, 1e-6), 1),
                                 totalFrames=N)
                self._flush()
                f += 1
        finally:
            if reader is not None:
                reader.stop_event.set()
