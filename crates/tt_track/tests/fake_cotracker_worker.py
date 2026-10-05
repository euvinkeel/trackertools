"""A stand-in for editor/cotracker_worker.py in tests (tests/cotracker_queue.rs):
the same protocol, no model, no torch, no graphics card (standard library only).

It says it is ready, reads the header and the frames, and answers each frame
at once with every query's own point (from the query's frame on). Switches in
the environment:
- `TT_FAKE_COTRACKER_FAIL=1`: fail at the start, as a worker whose PyTorch
  can't start does (a traceback on stderr, then `{"error": ..}`).
- `TT_FAKE_COTRACKER_DELAY=<seconds>`: wait this long on each frame, so jobs
  last long enough to overlap.
- `TT_FAKE_COTRACKER_HANG=load`: take a minute to "load the model";
  `=frames`: say it is ready, read the header, then read nothing for a
  minute (a worker stuck on the graphics card).
Arguments (`--weights ..`) are ignored.
"""

import json
import os
import sys
import time


def send(msg: dict):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def main():
    if os.environ.get("TT_FAKE_COTRACKER_FAIL") == "1":
        print("Traceback (most recent call last):", file=sys.stderr)
        print("RuntimeError: fake CoTracker failure (TT_FAKE_COTRACKER_FAIL)", file=sys.stderr)
        sys.stderr.flush()
        send({"error": "RuntimeError: fake CoTracker failure"})
        sys.exit(1)
    delay = float(os.environ.get("TT_FAKE_COTRACKER_DELAY") or 0)
    hang = os.environ.get("TT_FAKE_COTRACKER_HANG")
    if hang == "load":
        time.sleep(60)
    send({"ready": {"device": "fake", "window": 16}})
    stdin = sys.stdin.buffer
    header = json.loads(stdin.readline())
    if hang == "frames":
        time.sleep(60)
    size = int(header["width"]) * int(header["height"]) * 3
    queries = [(int(q[0]), float(q[1]), float(q[2])) for q in header["queries"]]
    i = 0
    while True:
        tag = stdin.read(1)
        if tag != b"F":
            break
        if len(stdin.read(size)) != size:
            raise EOFError("a frame was cut short")
        if delay:
            time.sleep(delay)
        send({"f": i, "points": [[x, y, 1.0] if i >= f else None for (f, x, y) in queries]})
        i += 1
    send({"done": True})


if __name__ == "__main__":
    main()
