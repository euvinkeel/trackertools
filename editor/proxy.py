"""Fast-scrub proxy media.

Browsers re-decode from the previous keyframe on every seek, so stepping a
single frame in a recording with a keyframe every few seconds costs hundreds
of milliseconds. Like other editors, we build a lightweight proxy (720p, a
keyframe every 12 frames, no B-frames, constant frame rate on the same frame
grid as the tracker) purely for display. Tracking always reads the original.
"""

import os
import queue
import shutil
import subprocess
import threading
from pathlib import Path
from typing import Callable, Dict

PROXY_HEIGHT = int(os.environ.get("COTRACK_PROXY_HEIGHT", "720"))
PROXY_GOP = 12
LONG_GOP = 30
BROWSER_CODECS = {"h264", "vp8", "vp9", "av1"}


def needs_proxy(meta: dict) -> bool:
    return meta.get("gop", 1) > LONG_GOP or meta.get("codec") not in BROWSER_CODECS


def ffmpeg_exe() -> str:
    exe = shutil.which("ffmpeg")
    if exe:
        return exe
    import imageio_ffmpeg

    return imageio_ffmpeg.get_ffmpeg_exe()


ENCODERS = [
    ("nvenc", ["-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-cq", "24", "-b:v", "0",
               "-maxrate", "8M", "-bufsize", "16M"]),
    ("x264", ["-c:v", "libx264", "-preset", "veryfast", "-tune", "fastdecode", "-crf", "23",
              "-maxrate", "8M", "-bufsize", "16M"]),
]


class ProxyManager:
    def __init__(self, proxy_dir: Path, probe: Callable[[str], dict],
                 on_ready: Callable[[str, dict], None]):
        self.dir = proxy_dir
        self.dir.mkdir(parents=True, exist_ok=True)
        self.probe = probe
        self.on_ready = on_ready
        self.jobs: Dict[str, dict] = {}
        self.lock = threading.Lock()
        self.tasks: "queue.Queue" = queue.Queue()
        threading.Thread(target=self._work, daemon=True).start()

    def status(self, video_id: str) -> dict:
        with self.lock:
            return dict(self.jobs.get(video_id) or {"state": "none", "progress": 0.0})

    def request(self, meta: dict):
        with self.lock:
            job = self.jobs.get(meta["id"])
            if job and job["state"] in ("queued", "building"):
                return
            self.jobs[meta["id"]] = {"state": "queued", "progress": 0.0}
        self.tasks.put(dict(meta))

    def _work(self):
        while True:
            meta = self.tasks.get()
            try:
                self._build(meta)
            except Exception as exc:
                with self.lock:
                    self.jobs[meta["id"]] = {"state": "error", "progress": 0.0, "message": str(exc)}

    def _set(self, video_id, **kw):
        with self.lock:
            self.jobs.setdefault(video_id, {}).update(kw)

    def _build(self, meta: dict):
        vid = meta["id"]
        out = self.dir / f"{vid}.mp4"
        tmp = self.dir / f"{vid}.part.mp4"
        self._set(vid, state="building", progress=0.0)
        vf = (f"fps={meta.get('fpsRational') or meta['fps']},"
              f"scale=-2:'min(ih,{PROXY_HEIGHT})':flags=bicubic,format=yuv420p")
        total = max(1, int(meta.get("frameCount") or 1))
        last_error = ""
        for name, enc in ENCODERS:
            cmd = [ffmpeg_exe(), "-hide_banner", "-nostdin", "-y", "-loglevel", "error"]
            if name == "nvenc":
                cmd += ["-hwaccel", "auto"]
            cmd += ["-i", meta["path"], "-map", "0:v:0", "-an", "-sn", "-dn", "-vf", vf, *enc,
                    "-g", str(PROXY_GOP), "-bf", "0", "-movflags", "+faststart",
                    "-progress", "pipe:1", str(tmp)]
            proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    text=True, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
            err_lines = []
            err_thread = threading.Thread(target=lambda: err_lines.extend(proc.stderr), daemon=True)
            err_thread.start()
            for line in proc.stdout:
                if line.startswith("frame="):
                    try:
                        self._set(vid, progress=min(0.999, int(line[6:]) / total), encoder=name)
                    except ValueError:
                        pass
            proc.wait()
            err_thread.join(timeout=2)
            if proc.returncode == 0 and tmp.exists():
                tmp.replace(out)
                info = self.probe(str(out))
                proxy = {"state": "ready", "path": str(out), "t0": info["t0"],
                         "frameCount": info["frameCount"], "width": info["width"],
                         "height": info["height"], "size": out.stat().st_size, "encoder": name}
                self.on_ready(vid, proxy)
                self._set(vid, state="ready", progress=1.0)
                return
            last_error = "".join(err_lines[-5:]).strip() or f"ffmpeg exited with {proc.returncode}"
            tmp.unlink(missing_ok=True)
        raise RuntimeError(last_error)
