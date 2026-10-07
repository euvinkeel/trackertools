"""A stand-in for editor/cotracker_worker.py in tests (tests/cotracker_queue.rs):
the same protocols, no model, no torch, no graphics card (standard library only).

It says it is ready, then answers each frame at once with every query's own
point (from the query's frame on). Plain: one header, its frames, `E`.
`--shared` (the app's): streams opened, fed and ended by number, mixed on
one input (see the real worker's docs). Switches in the environment:
- `TT_FAKE_COTRACKER_FAIL=1`: fail at the start, as a worker whose PyTorch
  can't start does (a traceback on stderr, then `{"error": ..}`).
- `TT_FAKE_COTRACKER_DELAY=<seconds>`: wait this long on each frame, so jobs
  last long enough to overlap.
- `TT_FAKE_COTRACKER_HANG=load`: take a minute to "load the model";
  `=frames`: say it is ready, then read nothing for a minute (a worker stuck
  on the graphics card).
- `TT_FAKE_COTRACKER_PIDS=<file>`: append this process's id to the file
  (tests count the workers started).
Arguments (`--weights ..`) are ignored.
"""

import json
import os
import sys
import time


def send(msg: dict):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def answer(i, queries):
    return [[x, y, 1.0] if i >= f else None for (f, x, y) in queries]


def plain(stdin, delay):
    header = json.loads(stdin.readline())
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
        send({"f": i, "points": answer(i, queries)})
        i += 1
    send({"done": True})


def shared(stdin, delay):
    streams = {}  # id -> [queries, size, frames seen]
    while True:
        tag = stdin.read(1)
        if not tag:
            return
        sid = int.from_bytes(stdin.read(4), "little")
        if tag == b"O":
            header = json.loads(stdin.readline())
            queries = [(int(q[0]), float(q[1]), float(q[2])) for q in header["queries"]]
            streams[sid] = [queries, int(header["width"]) * int(header["height"]) * 3, 0]
        elif tag == b"F":
            queries, size, i = streams.get(sid, [[], 512 * 384 * 3, 0])
            if len(stdin.read(size)) != size:
                raise EOFError("a frame was cut short")
            if sid in streams:
                if delay:
                    time.sleep(delay)
                send({"s": sid, "f": i, "points": answer(i, queries)})
                streams[sid][2] = i + 1
        elif tag == b"E":
            if streams.pop(sid, None) is not None:
                send({"s": sid, "done": True})
        elif tag == b"X":
            streams.pop(sid, None)


def main():
    pids = os.environ.get("TT_FAKE_COTRACKER_PIDS")
    if pids:
        with open(pids, "a") as f:
            f.write(f"{os.getpid()}\n")
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
    if hang == "frames":
        time.sleep(60)
    stdin = sys.stdin.buffer
    if "--shared" in sys.argv:
        shared(stdin, delay)
    else:
        plain(stdin, delay)


if __name__ == "__main__":
    main()
