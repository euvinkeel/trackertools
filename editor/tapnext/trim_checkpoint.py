"""Trim a TAPNext++ checkpoint to what tracking needs.

DeepMind's files (tapnextpp_ckpt.pt, tapnextpp_512.ckpt: 2.5 GB each) are
PyTorch Lightning training checkpoints: the model's weights (978 MB as fp32)
plus Adam's two moments for each (twice that again) and the training
config. Tracking needs the weights only, and not even all of them: the
`query_pos_embed` buffer (200 MB) is a fixed sin-cos table the model
rebuilds when it is constructed. `--fp16` halves the rest (the loader widens
it back to fp32: the error is the rounding of the stored weights only).

Saved as `{"tapnext": state_dict, "input_resolution": 256 | 512}` with
`torch.save`: plain tensors, loadable with `weights_only=True` (safetensors
would do the same job but isn't installed). The input resolution goes with
it because DeepMind's 256 checkpoint doesn't record it (tracker.read_weights).

Run: .venv\\Scripts\\python.exe editor\\tapnext\\trim_checkpoint.py SRC DST [--fp16] [--resolution 256|512]
"""

import argparse
import os
import sys

import torch

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from tapnext.tracker import DERIVED, read_weights  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("src")
    ap.add_argument("dst")
    ap.add_argument("--fp16", action="store_true", help="store the weights as fp16 (half the size)")
    ap.add_argument("--resolution", type=int, default=None, help="the input size it was trained at (default: from the checkpoint or its name)")
    args = ap.parse_args()
    sd, res = read_weights(args.src)
    dtype = torch.float16 if args.fp16 else torch.float32
    out = {k: v.to(dtype).contiguous().clone() for k, v in sd.items() if k not in DERIVED}
    tmp = args.dst + ".part"
    torch.save({"tapnext": out, "input_resolution": args.resolution or res}, tmp)
    os.replace(tmp, args.dst)
    n = sum(v.numel() for v in out.values())
    print(f"{args.dst}: {len(out)} tensors, {n / 1e6:.1f} M weights as {dtype}, input {args.resolution or res} px, {os.path.getsize(args.dst) / 1e6:.0f} MB (from {os.path.getsize(args.src) / 1e6:.0f} MB)")


if __name__ == "__main__":
    main()
