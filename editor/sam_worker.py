"""SAM 2.1 for trackertools' cursor trackers (tt_track `job::cursor`): which
pixels of a loose paint are the cursor.

A cursor is a few pixels across; SAM 2 is trained on objects far larger.
So the app sends the paint's pixels (a small crop) and this enlarges them
(`scale`, 8 by default) before SAM sees them: the cursor is then 100–300 px,
the size SAM handles well, and its masks are brought back to the crop's size.

Protocol, over stdin / stdout:
- stdout, once: `{"ready": {"device": ..}}` when the model is loaded (or
  `{"error": ..}` and the worker ends).
- stdin, per request: one JSON line `{"w": W, "h": H, "points": [[x, y,
  label], ...], "scale": K}` (crop pixels, continuous: (0, 0) is the
  top-left corner of the top-left pixel; label 1: the cursor, 0: not), then
  W × H × 3 bytes (RGB, row-major).
- stdout, per request: one JSON line `{"n": N, "scores": [..]}`, then N
  masks of W × H bytes (1: the cursor, 0: not), SAM's candidates (several
  sizes of object around the points), or `{"error": ..}` (the worker goes
  on).
stdin closing ends the worker.

Arguments: `--weights PATH` (`sam2.1_hiera_small.pt`; default the
`TT_SAM_WEIGHTS` environment variable), `--config` (SAM 2's config for those
weights, default `configs/sam2.1/sam2.1_hiera_s.yaml`), `--device cpu|cuda|mps`
(default CUDA when available, else Apple Silicon's GPU, else the CPU).
SAM 2 is Meta's, Apache-2.0 (code and weights); not in this repository: the
doctor installs the `sam2` package and downloads the weights.
"""

import argparse
import json
import os
import sys

import numpy as np
import torch


def send(msg: dict):
    sys.stdout.buffer.write((json.dumps(msg) + "\n").encode())
    sys.stdout.buffer.flush()


def pick_device(asked):
    if asked:
        return asked
    if torch.cuda.is_available():
        return "cuda"
    if getattr(torch.backends, "mps", None) is not None and torch.backends.mps.is_available():
        return "mps"
    return "cpu"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--weights", default=os.environ.get("TT_SAM_WEIGHTS"))
    ap.add_argument("--config", default="configs/sam2.1/sam2.1_hiera_s.yaml")
    ap.add_argument("--device", default=os.environ.get("TT_SAM_DEVICE"))
    args = ap.parse_args()
    try:
        from PIL import Image
        from sam2.build_sam import build_sam2
        from sam2.sam2_image_predictor import SAM2ImagePredictor

        device = pick_device(args.device)
        if not args.weights or not os.path.isfile(args.weights):
            raise FileNotFoundError(f"no SAM 2 weights at {args.weights!r}")
        model = build_sam2(args.config, args.weights, device=device)
        predictor = SAM2ImagePredictor(model)
    except Exception as exc:  # noqa: BLE001 (any failure to start is the answer)
        send({"error": f"SAM 2 does not start: {type(exc).__name__}: {exc}"})
        return
    send({"ready": {"device": device}})
    stdin = sys.stdin.buffer
    while True:
        line = stdin.readline()
        if not line:
            return
        try:
            req = json.loads(line)
            w, h, k = int(req["w"]), int(req["h"]), max(1, int(req.get("scale", 8)))
            data = stdin.read(w * h * 3)
            if len(data) != w * h * 3:
                return
            crop = np.frombuffer(data, np.uint8).reshape(h, w, 3)
            big = np.asarray(Image.fromarray(crop).resize((w * k, h * k), Image.BICUBIC))
            points = req.get("points") or []
            with torch.inference_mode():
                predictor.set_image(big)
                coords = np.array([[p[0] * k, p[1] * k] for p in points], np.float32) if points else None
                labels = np.array([int(p[2]) for p in points], np.int32) if points else None
                masks, scores, _ = predictor.predict(point_coords=coords, point_labels=labels, multimask_output=True)
            out = []
            for m in masks:
                # Back to the crop's size: a pixel is the cursor where most of it was.
                small = (m > 0).reshape(h, k, w, k).mean(axis=(1, 3)) >= 0.5
                out.append(small.astype(np.uint8).tobytes())
            send({"n": len(out), "scores": [float(s) for s in scores]})
            for b in out:
                sys.stdout.buffer.write(b)
            sys.stdout.buffer.flush()
        except Exception as exc:  # noqa: BLE001
            send({"error": f"{type(exc).__name__}: {exc}"})


if __name__ == "__main__":
    main()
