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
