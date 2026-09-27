"""Frame access on the constant-rate frame grid shared by the browser and the trackers.

Grid frame i is the decoded frame whose timestamp rounds to i / fps (after
subtracting the first frame's timestamp). Gaps (VFR) repeat the previous frame
so indices stay aligned with what the browser shows at that time.

- FrameReader streams frames sequentially on a background thread (tracking jobs).
- FrameServer serves exact frames on demand (template crops, the finetune loupe)
  from one warm decoder per video, so forward steps don't re-seek.
"""

import queue
import threading
from collections import OrderedDict
from typing import Callable, List, Optional

import av
import numpy as np


def grid_index(frame, fps: float, t0: float) -> Optional[int]:
    if frame.time is None:
        return None
    return int(round((frame.time - t0) * fps))


def seek_to(container, stream, f: int, fps: float, t0: float):
    target = t0 + (f - 0.5) / fps
    container.seek(max(0, int(target / stream.time_base)), stream=stream, backward=True)


_LUMA_FIRST = {"yuv420p", "yuvj420p", "yuv422p", "yuvj422p", "yuv444p", "yuvj444p", "nv12", "nv21", "gray"}


def to_gray(frame) -> np.ndarray:
    """Grayscale frame. For 8-bit YUV this is the luma plane as-is (~20x faster
    than a swscale conversion); normalized template matching doesn't care
    about the luma range."""
    if frame.format.name in _LUMA_FIRST:
        plane = frame.planes[0]
        rows = np.frombuffer(plane, np.uint8, count=frame.height * plane.line_size)
        rows = rows.reshape(frame.height, plane.line_size)
        return rows[:, :frame.width].copy()
    return frame.to_ndarray(format="gray")


def to_rgb(frame) -> np.ndarray:
    return frame.to_ndarray(format="rgb24")


class FrameReader:
    """Decodes a video on a background thread, from grid frame `start` on.

    `convert` turns a decoded av.VideoFrame into the array the consumer wants
    (e.g. model-resolution RGB for CoTracker, full-resolution gray for template
    matching)."""

    def __init__(self, path, fps, t0, start, stop_event, convert: Callable, maxsize=64):
        self.path = path
        self.fps = fps
        self.t0 = t0
        self.start = start
        self.stop_event = stop_event
        self.convert = convert
        self.queue: "queue.Queue" = queue.Queue(maxsize=maxsize)
        self.error: Optional[BaseException] = None
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _put(self, item) -> bool:
        while not self.stop_event.is_set():
            try:
                self.queue.put(item, timeout=0.1)
                return True
            except queue.Full:
                continue
        return False

    def _run(self):
        try:
            with av.open(self.path) as container:
                stream = container.streams.video[0]
                stream.thread_type = "AUTO"
                if self.start > 0:
                    seek_to(container, stream, self.start, self.fps, self.t0)
                expected = self.start
                previous = None
                for frame in container.decode(stream):
                    if self.stop_event.is_set():
                        return
                    idx = grid_index(frame, self.fps, self.t0)
                    if idx is None or idx < expected:
                        continue
                    img = self.convert(frame)
                    while expected < idx:
                        if not self._put((expected, previous if previous is not None else img)):
                            return
                        expected += 1
                    if not self._put((idx, img)):
                        return
                    previous = img
                    expected = idx + 1
        except BaseException as exc:  # surfaced to the job thread
            self.error = exc
        finally:
            self._put(None)

    def read(self, n: int) -> List[np.ndarray]:
        return [img for _, img in self.read_indexed(n)]

    def read_indexed(self, n: int) -> list:
        frames = []
        while len(frames) < n:
            item = self.queue.get()
            if item is None:
                self.queue.put(None)
                if self.error is not None:
                    raise self.error
                break
            frames.append(item)
        return frames


class _Decoder:
    """A persistent decoder positioned somewhere in one video."""

    def __init__(self, video: dict):
        self.fps = float(video["fps"])
        self.t0 = float(video.get("t0", 0.0))
        self.gop = int(video.get("gop", 60) or 60)
        self.container = av.open(video["path"])
        self.stream = self.container.streams.video[0]
        self.stream.thread_type = "AUTO"
        self.gen = None
        self.pos: Optional[int] = None  # smallest grid index servable without seeking
        self.prev = None  # (idx, av frame) last frame with idx < pos
        self.ahead = None  # (idx, av frame) decoded but beyond the last request

    def close(self):
        self.container.close()

    def _next(self):
        if self.ahead is not None:
            item, self.ahead = self.ahead, None
            return item
        for frame in self.gen:
            idx = grid_index(frame, self.fps, self.t0)
            if idx is not None:
                return idx, frame
        return None

    def frame(self, f: int):
        """The av.VideoFrame shown at grid index f (the last frame for f past the end)."""
        if self.gen is None or self.pos is None or f < self.pos or f - self.pos > self.gop:
            if f > 0:
                seek_to(self.container, self.stream, f, self.fps, self.t0)
            else:
                self.container.seek(0, stream=self.stream, backward=True)
            self.gen = self.container.decode(self.stream)
            self.prev = None
            self.ahead = None
        while True:
            item = self._next()
            if item is None:  # end of video: hold the last frame
                self.pos = f + 1
                if self.prev is None:
                    raise ValueError(f"frame {f} is not decodable")
                return self.prev[1]
            idx, frame = item
            if idx < f:
                self.prev = item
                continue
            self.pos = f + 1
            if idx == f:
                self.prev = item
                return frame
            self.ahead = item  # gap in the stream: frame f repeats the previous one
            return self.prev[1] if self.prev is not None else frame


class FrameServer:
    """Exact frames on demand, with an LRU cache of converted frames."""

    def __init__(self, cache_bytes: int = 384 << 20, max_decoders: int = 2):
        self.cache_bytes = cache_bytes
        self.max_decoders = max_decoders
        self._lock = threading.Lock()
        self._decoders: "OrderedDict[str, _Decoder]" = OrderedDict()
        self._cache: "OrderedDict[tuple, np.ndarray]" = OrderedDict()
        self._used = 0

    def _decoder(self, video: dict) -> _Decoder:
        key = video["path"]
        dec = self._decoders.get(key)
        if dec is None:
            dec = _Decoder(video)
            self._decoders[key] = dec
            while len(self._decoders) > self.max_decoders:
                _, old = self._decoders.popitem(last=False)
                old.close()
        self._decoders.move_to_end(key)
        return dec

    def get(self, video: dict, f: int, fmt: str = "rgb24") -> np.ndarray:
        f = max(0, min(int(f), int(video["frameCount"]) - 1))
        key = (video["path"], f, fmt)
        with self._lock:
            img = self._cache.get(key)
            if img is not None:
                self._cache.move_to_end(key)
                return img
            try:
                frame = self._decoder(video).frame(f)
            except Exception:
                old = self._decoders.pop(video["path"], None)
                if old is not None:
                    old.close()
                raise
            img = to_gray(frame) if fmt == "gray" else frame.to_ndarray(format=fmt)
            self._cache[key] = img
            self._used += img.nbytes
            while self._used > self.cache_bytes and len(self._cache) > 1:
                _, old_img = self._cache.popitem(last=False)
                self._used -= old_img.nbytes
            return img


def crop(img: np.ndarray, x: int, y: int, w: int, h: int, fill: int = 0) -> np.ndarray:
    """Region of img with its top-left at (x, y); parts outside the image are `fill`."""
    H, W = img.shape[:2]
    out = np.full((h, w) + img.shape[2:], fill, img.dtype)
    x0, y0, x1, y1 = max(0, x), max(0, y), min(W, x + w), min(H, y + h)
    if x1 > x0 and y1 > y0:
        out[y0 - y:y1 - y, x0 - x:x1 - x] = img[y0:y1, x0:x1]
    return out
