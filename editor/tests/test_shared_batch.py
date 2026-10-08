"""The shared CoTracker worker's batched rounds (editor/cotracker_worker.py,
`TT_COTRACKER_BATCH`) track exactly as its streams one at a time: the same 3
streams of real frames (crops of a fixture video, different point counts and
lengths, editor/bench_shared.py) with batching off and on, every point within
0.05 px and its probability within 1e-3. And a stream that fails in a batch
fails alone: the others go on to the same results; a batched step that fails
runs again stream by stream. On the CPU only, the real model in-process.
Skips without torch, the weights (scaled_online.pth in torch hub's cache,
or TT_COTRACKER_WEIGHTS) or the fixture video.

Run: .venv\\Scripts\\python.exe editor\\tests\\test_shared_batch.py [--video PATH]
"""
import os
import sys

# The CPU unless `--device mps|cuda` is given (on a Mac: mps). Never the
# graphics card by default: a card another app is using may reset.
DEVICE = sys.argv[sys.argv.index("--device") + 1] if "--device" in sys.argv else "cpu"
if DEVICE == "cpu":
    os.environ["CUDA_VISIBLE_DEVICES"] = ""
os.environ["TT_COTRACKER_DEVICE"] = DEVICE
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

try:
    import torch  # noqa: F401
except ImportError:
    print("skipped: no torch")
    sys.exit(0)

import bench_shared as bench  # noqa: E402
import cotracker_worker as cw  # noqa: E402

video = sys.argv[sys.argv.index("--video") + 1] if "--video" in sys.argv else bench.VIDEO
if not os.path.exists(cw.default_weights()):
    print(f"skipped: no weights at {cw.default_weights()}")
    sys.exit(0)
if not os.path.exists(video):
    print(f"skipped: no video at {video} (--video PATH)")
    sys.exit(0)

# Lengths: a partial last window (37), ending at a window's overlap (40), short (29).
LENGTHS = [37, 40, 29]
frames = bench.decode(video, max(LENGTHS))
data = bench.shared_input(frames, 3, LENGTHS)
eng = bench.load()


def by_stream(msgs):
    """Each stream's frames in order (asserted), whether it finished, its error."""
    out = {}
    for m in msgs:
        s = out.setdefault(m["s"], {"frames": [], "points": [], "done": False, "error": None})
        assert not s["done"], f"stream {m['s']} sent after done: {m}"
        if "f" in m:
            assert m["f"] == len(s["frames"]), f"stream {m['s']}: frame {m['f']} after {len(s['frames'])}"
            s["frames"].append(m["f"])
            s["points"].append(m["points"])
        elif m.get("done"):
            s["done"] = True
        elif "error" in m:
            s["error"] = m["error"]
    return out


def same(a, b, what):
    """Every point within 0.05 px, its probability within 1e-3; the largest gaps."""
    assert len(a["points"]) == len(b["points"]), (what, len(a["points"]), len(b["points"]))
    dxy = dp = 0.0
    for f, (pa, pb) in enumerate(zip(a["points"], b["points"])):
        assert len(pa) == len(pb), (what, f)
        for qa, qb in zip(pa, pb):
            assert (qa is None) == (qb is None), (what, f, qa, qb)
            if qa is not None:
                dxy = max(dxy, abs(qa[0] - qb[0]), abs(qa[1] - qb[1]))
                dp = max(dp, abs(qa[2] - qb[2]))
    assert dxy <= 0.05 and dp <= 1e-3, f"{what}: off by {dxy:.4f} px, probability {dp:.2e}"
    return dxy, dp


off = by_stream(bench.run(eng, data, batch=False)[0])
# (And see that the batches happen: several streams a round, point counts padded.)
batches = []
run_windows = cw.Engine.run_windows
cw.Engine.run_windows = lambda self, jobs: batches.append([len(j[1]) for j in jobs]) or run_windows(self, jobs)
try:
    on = by_stream(bench.run(eng, data, batch=True)[0])
finally:
    cw.Engine.run_windows = run_windows
assert any(len(set(b)) > 1 for b in batches), f"no padded batch: {batches}"
print(f"batched windows (points per stream): {batches}")
assert sorted(off) == sorted(on) == [1, 2, 3], (sorted(off), sorted(on))
for sid, n in zip((1, 2, 3), LENGTHS):
    assert off[sid]["done"] and on[sid]["done"] and not off[sid]["error"] and not on[sid]["error"], sid
    assert len(off[sid]["frames"]) == n, (sid, len(off[sid]["frames"]))
    dxy, dp = same(off[sid], on[sid], f"stream {sid}")
    print(f"stream {sid}: {n} frames, batched within {dxy:.2e} px, probability {dp:.2e}")

# One stream fails in a batch (its second window): it alone says so.
place = cw.Stream.place


def failing(self, pyr_new):
    if len(self.tracks) == 3 and self.ind == 8:  # stream 2 (3 queries)
        raise RuntimeError("a failure of stream 2's own")
    return place(self, pyr_new)


cw.Stream.place = failing
try:
    hurt = by_stream(bench.run(eng, data, batch=True)[0])
finally:
    cw.Stream.place = place
assert hurt[2]["error"] and "stream 2" in hurt[2]["error"] and not hurt[2]["done"], hurt[2]
assert hurt[2]["points"] == on[2]["points"][:len(hurt[2]["points"])]
for sid in (1, 3):
    assert hurt[sid]["done"] and not hurt[sid]["error"], sid
    same(off[sid], hurt[sid], f"stream {sid} beside a failing one")
print("a stream failing in a batch fails alone")

# A batched step that fails (the encoder, say, out of memory) runs stream by stream.
encode_many = cw.Engine.encode_many
cw.Engine.encode_many = lambda self, groups: (_ for _ in ()).throw(RuntimeError("the batch failed"))
try:
    fallback = by_stream(bench.run(eng, data, batch=True)[0])
finally:
    cw.Engine.encode_many = encode_many
for sid in (1, 2, 3):
    assert fallback[sid]["done"] and not fallback[sid]["error"], sid
    same(off[sid], fallback[sid], f"stream {sid}, batch failed")
print("a failed batch runs stream by stream")
print("ok")
