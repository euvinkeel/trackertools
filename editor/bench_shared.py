"""Where the shared CoTracker worker's time goes with several trackers at
once (editor/cotracker_worker.py, `--shared`), with its rounds batched and
not (`TT_COTRACKER_BATCH`): 1, 3 and 6 streams of real frames (512 × 384
crops of a fixture video at different places, a few queries each, one
joining late), sent the way the app sends them (each stream's frames in
turn) and run in-process through `run_shared`. On the CPU only: the
graphics card may be the app's (parallel CUDA work has reset its driver).

Run: .venv\\Scripts\\python.exe editor\\bench_shared.py [--frames 48]
     [--streams 1,3,6] [--repeat 1] [--video fixtures\\sprite_1080p60.mp4]
"""

import os
import sys

# The CPU only, and the worker's profile on (both before torch loads).
# The CPU unless `--device mps|cuda` is given (on a Mac: mps). Never the
# graphics card by default: a card another app is using may reset.
DEVICE = sys.argv[sys.argv.index("--device") + 1] if "--device" in sys.argv else "cpu"
if DEVICE == "cpu":
    os.environ["CUDA_VISIBLE_DEVICES"] = ""
os.environ["TT_COTRACKER_DEVICE"] = DEVICE
os.environ["TT_COTRACKER_PROFILE"] = "1"

import argparse  # noqa: E402
import contextlib  # noqa: E402
import io  # noqa: E402
import json  # noqa: E402
import sys  # noqa: E402
import time  # noqa: E402

import numpy as np  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cotracker_worker as cw  # noqa: E402
import torch  # noqa: E402

W, H = 512, 384
VIDEO = os.path.join(os.path.dirname(HERE), "fixtures", "sprite_1080p60.mp4")
# Crop corners (in a 1920 × 1080 frame; scaled into smaller ones), one per stream.
CORNERS = [(0, 0), (704, 348), (1408, 696), (300, 600), (1100, 120), (1408, 0)]
# Queries, crop px; streams take 2, 3 or 4 of them (different point counts).
QUERIES = [[0, 128.0, 96.0], [0, 256.0, 192.0], [5, 400.0, 300.0], [20, 200.0, 250.0]]


def decode(path: str, n: int) -> list:
    """The video's first `n` frames, RGB (PyAV, as the editor uses)."""
    import av

    out = []
    with av.open(path) as c:
        for frame in c.decode(video=0):
            out.append(frame.to_ndarray(format="rgb24"))
            if len(out) == n:
                break
    return out


def crops(frames: list, i: int) -> list:
    h, w = frames[0].shape[:2]
    x0, y0 = CORNERS[i % len(CORNERS)]
    x0, y0 = min(x0 * w // 1920, w - W), min(y0 * h // 1080, h - H)
    return [np.ascontiguousarray(f[y0:y0 + H, x0:x0 + W]) for f in frames]


def message(tag: bytes, sid: int, payload: bytes = b"") -> bytes:
    return tag + sid.to_bytes(4, "little") + payload


def shared_input(frames: list, n_streams: int, lengths=None) -> bytes:
    """The shared protocol's input for `n_streams` streams (numbered from 1):
    the headers, then the frames one stream after another (as the app's
    jobs send them), then each stream's end. `lengths`: frames per stream
    (default: all of them)."""
    lengths = lengths or [len(frames)] * n_streams
    out = io.BytesIO()
    per = []
    for i in range(n_streams):
        header = {"width": W, "height": H, "queries": QUERIES[:2 + i % 3]}
        out.write(message(b"O", i + 1, (json.dumps(header) + "\n").encode()))
        per.append(crops(frames[:lengths[i]], i))
    for f in range(max(lengths)):
        for i in range(n_streams):
            if f < lengths[i]:
                out.write(message(b"F", i + 1, per[i][f].tobytes()))
            if f == lengths[i] - 1:
                out.write(message(b"E", i + 1))
    return out.getvalue()


def run(eng, data: bytes, batch: bool):
    """`run_shared` over `data`: its messages (by stream), the wall time, and
    the profile's entries."""
    got = []
    cw.send = got.append
    cw.PROFILE_LOG.clear()
    t0 = time.perf_counter()
    with torch.inference_mode(), contextlib.redirect_stderr(io.StringIO()):
        cw.run_shared(eng, io.BytesIO(data), batch)
    return got, time.perf_counter() - t0, list(cw.PROFILE_LOG)


def load(device: str = DEVICE):
    torch.set_grad_enabled(False)
    eng = cw.Engine(cw.default_weights(), device)
    with torch.inference_mode(), contextlib.redirect_stderr(io.StringIO()):
        cw.practice(eng)
    return eng


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--frames", type=int, default=48)
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--streams", default="1,3,6")
    ap.add_argument("--video", default=VIDEO)
    ap.add_argument("--device", default="cpu", help="cpu (default), mps or cuda (read before torch loads)")
    args = ap.parse_args()
    frames = decode(args.video, args.frames)
    eng = load()
    print(f"{eng.device} ({torch.get_num_threads()} CPU threads); {len(frames)} frames a stream, windows of {eng.S}")
    print(f"{'streams':>7} {'batch':>5} {'wall s':>7} {'encode':>7} {'sample':>7} {'run':>7} {'other':>7} {'windows':>7} {'rounds':>6}  ms/frame/stream")
    for n in [int(s) for s in args.streams.split(",")]:
        data = shared_input(frames, n)
        for batch in (False, True):
            # The fastest of a few runs (the CPU is shared with whatever else runs).
            got, wall, log = min((run(eng, data, batch) for _ in range(args.repeat)), key=lambda r: r[1])
            assert not [m for m in got if "error" in m], [m for m in got if "error" in m]
            parts = {k: sum(e.get(k, 0.0) for e in log) for k in ("encode", "sample", "run")}
            windows = sum(e.get("streams", 1) for e in log)
            other = wall - sum(parts.values())
            print(f"{n:>7} {'on' if batch else 'off':>5} {wall:7.2f} {parts['encode']:7.2f} {parts['sample']:7.2f} {parts['run']:7.2f} {other:7.2f} {windows:>7} {len(log):>6}  {wall * 1000 / (n * len(frames)):.1f}")


if __name__ == "__main__":
    main()
