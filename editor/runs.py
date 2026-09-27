"""One tracking run = a CoTracker job (GPU worker) and/or a template job (own
thread), each with its own decoder. Results pass straight through (their keys
don't overlap); status messages are merged so the client sees a single run."""

import threading
from typing import Callable, Dict, List, Optional

from frames import FrameServer
from templates import TemplateJob, TemplateSegment

FINISHED = ("done", "halted", "error")


class RunJob:
    def __init__(self, emit: Callable[[dict], None]):
        self.emit = emit
        self.jobs: Dict[str, object] = {}
        self.states: Dict[str, dict] = {}
        self.preview_source: Optional[str] = None
        self.lock = threading.Lock()

    def sub_emit(self, name: str):
        def emit(m: dict):
            kind = m.get("type")
            if kind == "status":
                self._on_status(name, m)
            elif kind == "preview":
                if name == self.preview_source:
                    self.emit(m)
            else:
                self.emit(m)

        return emit

    def add(self, name: str, job):
        self.jobs[name] = job
        if self.preview_source is None or name == "points":
            self.preview_source = name

    def halt(self):
        for job in self.jobs.values():
            job.halt()

    def retire(self, entries):
        """Stop tracking the given segments at their cutoff frames (quality
        analysis confirmed an automatic end while the run is live)."""
        for job in self.jobs.values():
            if hasattr(job, "retire"):
                job.retire(entries)

    def _on_status(self, name: str, m: dict):
        with self.lock:
            self.states[name] = m
            merged = self._merge()
        self.emit(merged)

    def _merge(self) -> dict:
        states = [self.states.get(n, {"state": "starting"}) for n in self.jobs]
        out = {"type": "status"}
        starts = [s["startFrame"] for s in states if s.get("startFrame") is not None]
        if starts:
            out["startFrame"] = min(starts)
        totals = [s["totalFrames"] for s in states if s.get("totalFrames") is not None]
        if totals:
            out["totalFrames"] = max(totals)
        if all(s["state"] in FINISHED for s in states):
            errors = [s for s in states if s["state"] == "error"]
            if errors:
                out["state"] = "error"
                out["message"] = errors[0].get("message", "Tracking failed")
            else:
                out["state"] = "halted" if any(s["state"] == "halted" for s in states) else "done"
            frames = [s["frame"] for s in states if s.get("frame") is not None]
            if frames:
                out["frame"] = max(frames)
            return out
        out["state"] = "running"
        running = [s for s in states if s["state"] not in FINISHED]
        frames = [s["frame"] for s in running if s.get("frame") is not None]
        if frames:
            out["frame"] = min(frames)  # progress = the slowest job
        fps = [s["fps"] for s in running if s.get("fps")]
        if fps:
            out["fps"] = min(fps)
        return out


def start_run(engine, video: dict, start_frame: int, point_segments: List, template_segments: List[TemplateSegment],
              frames: FrameServer, emit: Callable[[dict], None], bounds: Optional[dict] = None) -> RunJob:
    run = RunJob(emit)
    if point_segments:
        run.add("points", None)
    if template_segments:
        run.add("templates", None)
    if not run.jobs:
        emit({"type": "status", "state": "done", "frame": start_frame, "startFrame": start_frame})
        return run
    if point_segments:
        run.jobs["points"] = engine.start_job(video, start_frame, point_segments, run.sub_emit("points"), bounds)
    if template_segments:
        run.jobs["templates"] = TemplateJob(video, start_frame, template_segments, frames,
                                            run.sub_emit("templates"), bounds).start()
    return run
