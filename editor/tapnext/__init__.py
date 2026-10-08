"""TAPNext++ (Google DeepMind, arXiv 2604.10582): the "precision" tracker
method, run inside the CoTracker worker (editor/cotracker_worker.py).

`tapnext_torch.py`, `tapnext_lru_modules.py` and `pscan.py` are DeepMind's
PyTorch TAPNext, vendored (Apache-2.0: LICENSE, NOTICE); `tracker.py` is
ours: loading, the crop ↔ model mapping, and one online stream.
"""
