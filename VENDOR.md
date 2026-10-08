# Vendored dependencies

## CoTracker3

The following files are copied verbatim from
[facebookresearch/co-tracker](https://github.com/facebookresearch/co-tracker)
at commit `82e02e8029753ad4ef13cf06be7f4fc5facdda4d`
("release kubric dataset used for cotracker3 training"):

- `cotracker/` — the model package (`models/`, `predictor.py`, `utils/`, …)
- `hubconf.py` — the `torch.hub` entry point the engine loads
- `setup.py` — package metadata for the editable install
- `LICENSE.md` — the upstream license

`__pycache__` and build artifacts from the original checkout are not included.

### License

The upstream code and the CoTracker3 **model weights** are licensed
**Attribution-NonCommercial 4.0 International (CC-BY-NC 4.0)** — non-commercial
use only. See `LICENSE.md`. The weights (`scaled_online.pth`) are downloaded on
first run to `%USERPROFILE%\.cache\torch\hub\checkpoints\`.

### Updating the vendored copy

Re-copy the files above from a newer co-tracker checkout, then reinstall the
editable package:

```powershell
.venv\Scripts\python.exe -m pip install -e . --no-build-isolation --no-deps
```

## TAPNext++ (PyTorch TAPNext)

`editor/tapnext/tapnext_torch.py`, `tapnext_lru_modules.py` and `pscan.py`
are from [google-deepmind/tapnet](https://github.com/google-deepmind/tapnet)
(`tapnet/tapnext/`) at commit `148b2c4090eb2e34551c46cf079be258ea41d175`
("Support variable resolutions and add VOTSp2026 submission files"), with
package-relative imports and einops replaced by plain reshapes (no einops
dependency); `editor/tapnext/NOTICE` lists the changes, `LICENSE` is the
upstream one. `tracker.py` and `trim_checkpoint.py` there are ours.

### License

Code and the TAPNext++ weights are **Apache-2.0**. The weights are not in
this repository: `tapnextpp_ckpt.pt` (256² input) and `tapnextpp_512.ckpt`
(512² input), 2.5 GB each as published (training checkpoints); trim them
with `editor/tapnext/trim_checkpoint.py`.

### Updating the vendored copy

Download the three files from a newer tapnet commit and reapply the
changes listed in `editor/tapnext/NOTICE`; run `editor/tests/test_tapnext.py`.
