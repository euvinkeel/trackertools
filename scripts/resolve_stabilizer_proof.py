"""Proves the Resolve stabilizer export in DaVinci Resolve itself.

trackertools' *Copy Resolve stabilizer (Fusion)* (tt_track::export) is pasted
into Resolve's Fusion page the way a person does it (the clip's comp shown,
MediaIn1 selected, paste), Resolve renders the timeline, and the render is
measured: patches of one stabilized frame are found again on every other frame
(normalized cross-correlation, sub-pixel), and the rigid motion that best
explains them gives the rotation left over. A stabilized render whose patches
stay put and whose rotation stays at zero is stabilized, by measurement.

Cases: the synthetic clips `cargo test -p tt_track --release --test stabilize`
leaves in the target's tmp folder (a still scene, a camera drifting up to 103 px
and rolling up to 5.7°, two points tracked by trackertools) placed as people
place clips: untrimmed, trimmed, on a timeline of another rate, inside a Fusion
Clip. Two controls are expected to fail (the export before it found its own
frame, on a trimmed clip; a Fusion Clip whose start was left unset), to show the
measurement catches a stabilizer that is off. `--real` adds a real clip with the
setting `export_a_saved_projects_stabilizer` (same test file) made from a saved
project.

Needs: Resolve running with the in-app bridge of davinci-resolve-mcp started
(Workspace > Scripts > resolve_bridge; free or Studio), that checkout
(RESOLVE_MCP_DIR, default ~/Documents/repos/davinci-resolve-mcp), ffmpeg, and
numpy + opencv (the repo's .venv has both):

    .venv\\Scripts\\python.exe scripts\\resolve_stabilizer_proof.py
    .venv\\Scripts\\python.exe scripts\\resolve_stabilizer_proof.py --real <video> <setting> [--real-fusion-clip]

It saves the project that is open, works in a project of its own ("trackertools
stabilizer proof", kept so its timelines can be looked at), and reopens the
first project when done. Renders, previews and report.json go to
target/resolve-proof/.
"""

from __future__ import annotations

import argparse
import glob
import json
import math
import os
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

import cv2
import numpy as np

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "target" / "resolve-proof"
SCRATCH = "trackertools stabilizer proof"
MCP = Path(os.environ.get("RESOLVE_MCP_DIR", Path.home() / "Documents" / "repos" / "davinci-resolve-mcp"))
# Where the stabilize test leaves its clips and settings (CARGO_TARGET_TMPDIR).
TEST_TMP = [REPO / "target" / "bench" / "tmp", REPO / "target" / "tmp"]


# ---------------------------------------------------------------- Resolve


def connect():
    sys.path.insert(0, str(MCP))
    try:
        from src.utils.resolve_bridge_client import connect as bridge  # type: ignore
    except ImportError as e:
        raise SystemExit(f"davinci-resolve-mcp not found at {MCP} (set RESOLVE_MCP_DIR): {e}")
    return bridge(timeout=900, require_enabled=False)


def hidden(obj, name, *args):
    """Call a Fusion method the bridge's proxy doesn't list (Inputs and Outputs hide some)."""
    from src.utils.resolve_bridge_client import _BoundMethod  # type: ignore
    return _BoundMethod(obj._transport, obj._handle, name)(*args)


class Scratch:
    """Saves the open project, works in SCRATCH, reopens the first project on exit."""

    def __init__(self, r, stay=False):
        self.r, self.stay, self.back = r, stay, None

    def __enter__(self):
        pm = self.r.GetProjectManager()
        cur = pm.GetCurrentProject()
        if cur and cur.GetName() != SCRATCH:
            tl = cur.GetCurrentTimeline()
            self.back = (cur.GetName(), tl.GetName() if tl else None, self.r.GetCurrentPage())
            if not pm.SaveProject():
                raise SystemExit(f"could not save {cur.GetName()!r}; not switching projects")
            print(f"saved {cur.GetName()!r}; switching to {SCRATCH!r}")
            names = pm.GetProjectListInCurrentFolder() or []
            if not (pm.LoadProject(SCRATCH) if SCRATCH in names else pm.CreateProject(SCRATCH)):
                raise SystemExit(f"could not open {SCRATCH!r}")
        return pm.GetCurrentProject()

    def __exit__(self, *exc):
        if self.stay or not self.back:
            return False
        pm = self.r.GetProjectManager()
        pm.SaveProject()
        name, timeline, page = self.back
        p = pm.LoadProject(name)
        if not p:
            print(f"!! could not reopen {name!r}: open it from the Project Manager")
            return False
        for i in range(1, (p.GetTimelineCount() or 0) + 1):
            t = p.GetTimelineByIndex(i)
            if t and t.GetName() == timeline:
                p.SetCurrentTimeline(t)
        if page:
            self.r.OpenPage(page)
        print(f"reopened {name!r} ({timeline}, {page} page)")
        return False


def media(p, path):
    root = p.GetMediaPool().GetRootFolder()
    for c in root.GetClipList() or []:
        if (c.GetClipProperty("File Path") or "").lower() == str(path).lower():
            return c
    items = p.GetMediaPool().ImportMedia([str(path)]) or []
    if not items:
        raise SystemExit(f"Resolve could not import {path}")
    return items[0]


def new_timeline(p, name, fps, size):
    mp = p.GetMediaPool()
    for i in range(1, (p.GetTimelineCount() or 0) + 1):
        t = p.GetTimelineByIndex(i)
        if t and t.GetName() == name:
            mp.DeleteTimelines([t])
            break
    t = mp.CreateEmptyTimeline(name)
    p.SetCurrentTimeline(t)
    for k, v in (("useCustomSettings", "1"), ("timelineFrameRate", str(fps)),
                 ("timelineResolutionWidth", str(size[0])), ("timelineResolutionHeight", str(size[1]))):
        t.SetSetting(k, v)
    got = (float(t.GetSetting("timelineFrameRate")), int(t.GetSetting("timelineResolutionWidth")), int(t.GetSetting("timelineResolutionHeight")))
    if got != (float(fps), size[0], size[1]):
        raise SystemExit(f"timeline {name}: asked for {fps} fps {size}, got {got}")
    return t


def render(r, p, t, path: Path, fmt, codec, quality=None):
    """Renders all of `t` to `path` (no extension; Resolve adds it). Returns the file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    for f in glob.glob(str(path) + ".*"):
        os.remove(f)
    p.SetCurrentTimeline(t)
    if not p.SetCurrentRenderFormatAndCodec(fmt, codec):
        raise SystemExit(f"Resolve refused {fmt}/{codec}")
    settings = {"SelectAllFrames": True, "TargetDir": str(path.parent), "CustomName": path.name,
                "ExportVideo": True, "ExportAudio": False}
    if quality:
        settings["VideoQuality"] = quality
    if not p.SetRenderSettings(settings):
        raise SystemExit("Resolve refused the render settings")
    job = p.AddRenderJob()
    t0 = time.time()
    p.StartRendering([job], False)
    while p.IsRenderingInProgress():
        time.sleep(0.3)
    status = p.GetRenderJobStatus(job) or {}
    p.DeleteRenderJob(job)
    files = glob.glob(str(path) + ".*")
    if status.get("JobStatus") != "Complete" or not files:
        raise SystemExit(f"render of {path.name} failed: {status}")
    print(f"   rendered {path.name} in {time.time() - t0:.1f} s")
    return Path(files[0])


def tool_name(tool):
    return (tool.GetAttrs() or {}).get("TOOLS_Name")


def feeding(tool, input_id="Input"):
    for _, inp in (tool.GetInputList() or {}).items():
        if (hidden(inp, "GetAttrs") or {}).get("INPS_ID") == input_id:
            out = hidden(inp, "GetConnectedOutput")
            return tool_name(hidden(out, "GetTool")) if out else None
    return None


def paste(r, p, t, setting: Path):
    """Pastes `setting` as a person does: the clip's comp on the Fusion page, MediaIn1
    selected, paste (Fusion only pastes into the comp on screen). Returns (comp, wiring)."""
    p.SetCurrentTimeline(t)
    t.SetCurrentTimecode(t.GetStartTimecode())
    r.OpenPage("fusion")
    time.sleep(1.0)
    comp = r.Fusion().GetCurrentComp()
    item = t.GetItemListInTrack("video", 1)[0]
    if (comp.GetAttrs() or {}).get("COMPS_Name") != (item.GetFusionCompByIndex(1).GetAttrs() or {}).get("COMPS_Name"):
        raise SystemExit("the Fusion page isn't showing this clip's comp")
    # Execute is asynchronous: wait for the result.
    comp.Execute(f'''
for _, tool in pairs(comp:GetToolList(false)) do
  if tool.Name:sub(1, 9) == "Stabilize" then tool:Delete() end
end
comp:FindTool("MediaOut1"):ConnectInput("Input", comp:FindTool("MediaIn1"))
comp:SetActiveTool(comp:FindTool("MediaIn1"))
comp:Paste(bmd.readfile("{setting.as_posix()}"))
''')
    for _ in range(120):
        time.sleep(0.25)
        if comp.FindTool("Stabilize"):
            break
    else:
        raise SystemExit(f"pasting {setting.name} made no Stabilize tool")
    time.sleep(0.5)
    st, mo = comp.FindTool("Stabilize"), comp.FindTool("MediaOut1")
    return comp, {"Stabilize.Input": feeding(st), "MediaOut1.Input": feeding(mo)}


# ---------------------------------------------------------------- measuring


def subpixel(a, b, c):
    d = a - 2 * b + c
    return 0.5 * (a - c) / d if d < 0 else 0.0


def find(ref, img, p, half, search, min_score):
    """Where the patch of `ref` around p is in `img` (displacement, px), or None; a
    best match on the search window's edge comes back as infinitely far (it is at
    least `search` away, and must not pass for a near match)."""
    x, y = int(round(p[0])), int(round(p[1]))
    tpl = ref[y - half:y + half + 1, x - half:x + half + 1]
    win = img[y - half - search:y + half + search + 1, x - half - search:x + half + search + 1]
    n = 2 * half + 1
    if tpl.shape != (n, n) or win.shape != (n + 2 * search, n + 2 * search) or tpl.std() < 2:
        return None
    res = cv2.matchTemplate(win, tpl, cv2.TM_CCOEFF_NORMED)
    _, score, _, (j, i) = cv2.minMaxLoc(res)
    if score < min_score:
        return None
    if not (0 < j < res.shape[1] - 1 and 0 < i < res.shape[0] - 1):
        return np.array([np.inf, np.inf])
    dx = subpixel(res[i, j - 1], res[i, j], res[i, j + 1])
    dy = subpixel(res[i - 1, j], res[i, j], res[i + 1, j])
    return np.array([j + dx - search, i + dy - search])


def rigid(points, moved):
    """The rotation (degrees, counter-clockwise on screen) of the best rigid fit points → moved."""
    P, Q = np.asarray(points, float), np.asarray(moved, float)
    P, Q = P - P.mean(0), Q - Q.mean(0)
    # Image y is down: flip it so the angle reads as on screen, counter-clockwise positive.
    P[:, 1] *= -1
    Q[:, 1] *= -1
    return math.degrees(math.atan2((P[:, 0] * Q[:, 1] - P[:, 1] * Q[:, 0]).sum(), (P * Q).sum()))


def frames(path):
    cap = cv2.VideoCapture(str(path))
    while True:
        ok, f = cap.read()
        if not ok:
            return
        yield cv2.cvtColor(f, cv2.COLOR_BGR2GRAY)


def measure(path, ref_index, points, half, search, min_score):
    """Every frame against frame `ref_index`: each point's displacement and the
    rotation of the rigid fit through all points found."""
    ref = next(f for i, f in enumerate(frames(path)) if i == ref_index)
    disp, rot, lost, far = [], [], 0, 0
    for img in frames(path):
        found = [(p, find(ref, img, p, half, search, min_score)) for p in points]
        found = [(p, d) for p, d in found if d is not None]
        lost += len(points) - len(found)
        far += sum(1 for _, d in found if not np.isfinite(d).all())
        disp.append([float(np.hypot(*d)) for _, d in found])
        near = [(p, d) for p, d in found if np.isfinite(d).all()]
        rot.append(rigid([p for p, _ in near], [np.add(p, d) for p, d in near]) if len(near) >= 2 else float("nan"))
    worst = np.array([max(d) if d else np.nan for d in disp])
    rot = np.abs(np.array(rot))
    return {
        "frames": len(disp),
        "points": len(points),
        "not_found": lost,
        f"beyond_{search}px": far,
        "drift_px": {"median": float(np.nanmedian(worst)), "p95": float(np.nanpercentile(worst, 95)), "max": float(np.nanmax(worst))},
        "rotation_deg": {"median": float(np.nanmedian(rot)), "p95": float(np.nanpercentile(rot, 95)), "max": float(np.nanmax(rot))},
        "per_frame_worst_px": [round(float(v), 3) for v in worst],
        "per_frame_rotation_deg": [round(float(v), 4) for v in rot],
    }


def similarity(kp_a, des_a, kp_b, des_b, matcher):
    """The rotation + uniform scale + shift (RANSAC over SIFT matches) taking a to b, or None."""
    if des_a is None or des_b is None or len(kp_a) < 8 or len(kp_b) < 8:
        return None
    good = [m for m, n in (p for p in matcher.knnMatch(des_a, des_b, k=2) if len(p) == 2) if m.distance < 0.75 * n.distance]
    if len(good) < 12:
        return None
    src = np.float32([kp_a[m.queryIdx].pt for m in good])
    dst = np.float32([kp_b[m.trainIdx].pt for m in good])
    M, inliers = cv2.estimateAffinePartial2D(src, dst, method=cv2.RANSAC, ransacReprojThreshold=2.0, maxIters=5000, confidence=0.999)
    if M is None or int(inliers.sum()) < 12:
        return None
    return M, int(inliers.sum())


def measure_real(path, wanted, ref_index, roi, anchor):
    """Real footage moves in depth, so patches change size: instead, SIFT features of the
    region `roi` (render px) of the reference frame, and of the frame before, are found
    on each frame and a rotation + scale + shift is fitted (RANSAC). A position and
    rotation stabilizer should leave the rotation at 0 and `anchor` (the two tracked
    points' midpoint) where it is; the scale is free. `wanted`: the output frames to look
    at (one per source frame)."""
    sift, matcher = cv2.SIFT_create(nfeatures=3000), cv2.BFMatcher(cv2.NORM_L2)
    x0, y0, x1, y1 = (int(v) for v in roi)

    def region(img):
        mask = np.zeros_like(img)
        mask[max(0, y0):y1, max(0, x0):x1] = 255
        return sift.detectAndCompute(img, mask)

    kr, dr = region(next(f for i, f in enumerate(frames(path)) if i == ref_index))
    ax, ay = anchor
    rot, scale, moved, step_rot, step_moved, missed, prev = [], [], [], [], [], 0, None
    for i, img in enumerate(frames(path)):
        if i not in wanted:
            continue
        kp, des = sift.detectAndCompute(img, None)
        fit = similarity(kr, dr, kp, des, matcher)
        if fit is None:
            missed += 1
        else:
            M = fit[0]
            # Image y is down: negate so counter-clockwise on screen reads positive.
            rot.append(-math.degrees(math.atan2(M[1, 0], M[0, 0])))
            scale.append(math.hypot(M[0, 0], M[1, 0]))
            moved.append(float(np.hypot(*(M @ [ax, ay, 1.0] - [ax, ay]))))
        if prev is not None:
            step = similarity(prev[0], prev[1], kp, des, matcher)
            if step is not None:
                S = step[0]
                step_rot.append(abs(math.degrees(math.atan2(S[1, 0], S[0, 0]))))
                step_moved.append(float(np.hypot(*(S @ [ax, ay, 1.0] - [ax, ay]))))
        prev = region(img)
    rot, moved = np.abs(np.array(rot)), np.array(moved)

    def stats(v):
        return {"median": float(np.median(v)), "p95": float(np.percentile(v, 95)), "max": float(np.max(v))}

    return {
        "frames": len(wanted), "not_fitted": missed,
        "anchor_moved_px": stats(moved), "rotation_deg": stats(rot),
        "scale": {"min": float(np.min(scale)), "max": float(np.max(scale))},
        "frame_to_frame_rotation_deg": stats(np.array(step_rot)), "frame_to_frame_anchor_px": stats(np.array(step_moved)),
        "per_frame_rotation_deg": [round(float(v), 4) for v in rot], "per_frame_anchor_px": [round(float(v), 3) for v in moved],
    }


def preview(raw, stab, out, width, marks=()):
    """raw | stabilized, side by side; `marks` (x, y in render px) boxed on the stabilized side."""
    sx = width / cv2.VideoCapture(str(raw)).get(cv2.CAP_PROP_FRAME_WIDTH)
    draw = "".join(f",drawbox=x={int(x * sx) - 10}:y={int(y * sx) - 10}:w=20:h=20:color=red@0.9:t=2" for x, y in marks)
    graph = f"[0:v]scale={width}:-2[a];[1:v]scale={width}:-2{draw}[b];[a][b]hstack"
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(raw), "-i", str(stab),
                    "-filter_complex", graph, "-c:v", "libx264", "-crf", "18", "-pix_fmt", "yuv420p", str(out)], check=True)


# ---------------------------------------------------------------- cases


@dataclass
class Case:
    name: str
    clip: Path
    setting: Path
    timeline_fps: float
    size: tuple
    start: int | None = None  # the clip's first source frame (a trim)
    end: int | None = None
    fusion_clip: bool = False
    clip_start: int | None = None  # set "Clip Starts At Source Frame" after pasting
    reference: int = 0  # source frame held still
    source_fps: float = 30.0
    points: list = field(default_factory=list)  # render px
    roi: tuple | None = None  # real clips: the region measured (render px; the tracked points' surroundings)
    control: bool = False  # expected to fail
    real: bool = False

    def output_frame_of(self, source_frame):
        """The first output frame showing `source_frame` (Resolve picks floor(k × source / timeline fps))."""
        s0 = self.start or 0
        k = math.ceil((source_frame - s0) * self.timeline_fps / self.source_fps - 1e-9)
        while s0 + math.floor(k * self.source_fps / self.timeline_fps + 1e-9) < source_frame:
            k += 1
        return k


def old_format(setting: Path, out: Path):
    """The same keys as the export wrote them before it found its own frame: Center
    and Angle keyed straight on the source's frame numbers (the format of 04fd8da)."""
    text = setting.read_text()
    splines = text[text.index("\t\tStabilizeSourceX"):text.index("\t\tStabilize = Transform")]
    splines = (splines.replace("StabilizeSourceX", "StabilizeCenterX").replace("StabilizeSourceY", "StabilizeCenterY")
               .replace("StabilizeSourceAngle", "StabilizeAngle"))
    out.write_text(
        "{\n\tTools = ordered() {\n" + splines
        + '\t\tStabilizeCenter = XYPath {\n\t\t\tShowKeyPoints = false,\n\t\t\tDrawMode = "ModifyOnly",\n\t\t\tInputs = {\n'
        '\t\t\t\tX = Input { SourceOp = "StabilizeCenterX", Source = "Value", },\n'
        '\t\t\t\tY = Input { SourceOp = "StabilizeCenterY", Source = "Value", },\n\t\t\t},\n\t\t},\n'
        '\t\tStabilize = Transform {\n\t\t\tNameSet = true,\n\t\t\tInputs = {\n'
        '\t\t\t\tCenter = Input { SourceOp = "StabilizeCenter", Source = "Value", },\n'
        '\t\t\t\tAngle = Input { SourceOp = "StabilizeAngle", Source = "Value", },\n\t\t\t},\n'
        '\t\t\tViewInfo = OperatorInfo { Pos = { 220, 50 } },\n\t\t},\n\t},\n\tActiveTool = "Stabilize"\n}\n')
    return out


def synthetic_cases():
    def newest(name):
        found = [d / name for d in TEST_TMP if (d / name).exists()]
        if not found:
            raise SystemExit(f"{name} not found: run `cargo test -p tt_track --release --test stabilize` first")
        return max(found, key=lambda f: f.stat().st_mtime)

    grid = [(x, y) for x in range(160, 801, 80) for y in range(150, 391, 60)]
    c30, s30 = newest("stabilize_540p30.mp4"), newest("stabilize_30.setting")
    c50, s50 = newest("stabilize_540p50.mp4"), newest("stabilize_50.setting")
    old = [old_format(s30, OUT / "old_format_30.setting")]
    size = (960, 540)
    cases = [
        Case("A  30 fps clip, 30 fps timeline", c30, s30, 30, size, reference=40, source_fps=30, points=grid),
        Case("B  30 fps clip trimmed to start on frame 25", c30, s30, 30, size, 25, 149, reference=40, source_fps=30, points=grid),
        Case("C  50 fps clip trimmed to frame 30, 60 fps timeline", c50, s50, 60, size, 30, 249, reference=67, source_fps=50, points=grid),
        Case("D  same, inside a Fusion Clip, start set to 30", c50, s50, 60, size, 30, 249, True, 30, reference=67, source_fps=50, points=grid),
        Case("E  control: Fusion Clip, start left unset", c50, s50, 60, size, 30, 249, True, None, reference=67, source_fps=50, points=grid, control=True),
        Case("F  control: the old export, trimmed clip", c30, old[0], 30, size, 25, 149, reference=40, source_fps=30, points=grid, control=True),
    ]
    return cases


def real_case(video, setting, fusion_clip, roi=None):
    info = json.loads(Path(setting).with_suffix(".json").read_text())
    # The clip's own size (it may be a smaller encode of the tracked file): measured at full resolution.
    cap = cv2.VideoCapture(str(video))
    size = (int(cap.get(cv2.CAP_PROP_FRAME_WIDTH)), int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT)))
    sx = size[0] / info["size"][0]
    points = [(x * sx, y * sx) for x, y in info["points"]]
    if roi is None:  # a box around the points (one or more), four times their extent on each side
        xs, ys = [q[0] for q in info["points"]], [q[1] for q in info["points"]]
        r = 4 * max(math.hypot(max(xs) - min(xs), max(ys) - min(ys)), 50)
        cx, cy = sum(xs) / len(xs), sum(ys) / len(ys)
        roi = (cx - r, cy - r, cx + r, cy + r)
    roi = tuple(v * sx for v in roi)
    start, end = info["first"], info["last"]
    name = ("S  real clip, inside a Fusion Clip, start set" if fusion_clip else "R  real clip, trimmed to the tracked frames") + ", 60 fps timeline"
    return Case(name, Path(video), Path(setting), 60, size, start, end, fusion_clip, start if fusion_clip else None,
                reference=info["reference"], source_fps=info["fps"], points=points, roi=roi, real=True)


def make(r, p, case: Case, d: Path):
    """Puts the clip on a timeline as the case says, renders it raw, pastes the setting,
    renders it stabilized. Returns (raw, stabilized, whether the paste wired itself in)."""
    t = new_timeline(p, f"proof {case.name.split()[0]}", case.timeline_fps, case.size)
    info = {"mediaPoolItem": media(p, case.clip), "mediaType": 1, "trackIndex": 1}
    if case.start is not None:
        info.update(startFrame=case.start, endFrame=case.end)
    if not p.GetMediaPool().AppendToTimeline([info]) or not t.GetItemListInTrack("video", 1):
        raise SystemExit(f"could not place {case.clip.name} on a timeline (does Resolve decode it? free Resolve "
                         "on Windows doesn't decode 10-bit HEVC: re-encode it, keeping every frame)")
    if case.fusion_clip:
        t.CreateFusionClip([t.GetItemListInTrack("video", 1)[0]])
    item = t.GetItemListInTrack("video", 1)[0]
    if not (item.GetFusionCompCount() or 0):
        item.AddFusionComp()
    fmt = ("mp4", "H264", None) if case.real else ("mov", "FFV1YUV420_8", None)
    raw = render(r, p, t, d / "raw", *fmt)
    comp, wiring = paste(r, p, t, case.setting)
    st = comp.FindTool("Stabilize")
    if case.clip_start is not None:
        st.SetInput("ClipStart", case.clip_start)
    wired = wiring == {"Stabilize.Input": "MediaIn1", "MediaOut1.Input": "Stabilize"}
    if not wired:
        print(f"   !! the paste didn't wire itself in: {wiring}; wiring it by hand to measure")
        st.ConnectInput("Input", comp.FindTool("MediaIn1"))
        comp.FindTool("MediaOut1").ConnectInput("Input", st)
    stab = render(r, p, t, d / "stabilized", *fmt)
    return raw, stab, wired


def evaluate(case: Case, raw: Path, stab: Path, wired):
    ref = case.output_frame_of(case.reference)
    result = {"case": case.name, "control": case.control, "wiring_by_paste": wired, "reference_output_frame": ref}
    if case.real:
        wanted = {case.output_frame_of(f) for f in range(case.start, case.end + 1)}
        anchor = tuple(np.mean(case.points, axis=0))
        result["raw"] = measure_real(raw, wanted, ref, case.roi, anchor)
        result["stabilized"] = measure_real(stab, wanted, ref, case.roi, anchor)
        for key in ("raw", "stabilized"):
            m = result[key]
            print(f"   {key + ':':11s} tracked midpoint moves median {m['anchor_moved_px']['median']:.2f} px, p95 {m['anchor_moved_px']['p95']:.2f}, max {m['anchor_moved_px']['max']:.2f}; "
                  f"turned median {m['rotation_deg']['median']:.3f}°, p95 {m['rotation_deg']['p95']:.3f}°, max {m['rotation_deg']['max']:.3f}°; "
                  f"frame to frame: turn p95 {m['frame_to_frame_rotation_deg']['p95']:.3f}°, move p95 {m['frame_to_frame_anchor_px']['p95']:.2f} px; "
                  f"scale {m['scale']['min']:.3f}–{m['scale']['max']:.3f} ({m['not_fitted']} of {m['frames']} frames not fitted)")
    else:
        result["stabilized"] = measure(stab, ref, case.points, 20, 40, 0.7)
        result["raw"] = measure(raw, ref, case.points, 20, 150, 0.3)
        s, w = result["stabilized"], result["raw"]
        print(f"   raw:        drift median {w['drift_px']['median']:.1f} px, max {w['drift_px']['max']:.1f} px; rotation max {w['rotation_deg']['max']:.2f}°")
        print(f"   stabilized: drift median {s['drift_px']['median']:.2f} px, p95 {s['drift_px']['p95']:.2f}, max {s['drift_px']['max']:.2f} px; "
              f"rotation median {s['rotation_deg']['median']:.3f}°, max {s['rotation_deg']['max']:.3f}°  "
              f"({s['not_found']} of {s['frames'] * s['points']} patches not found, {s['beyond_40px']} more than 40 px away)")
    return result


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--real", nargs=2, metavar=("VIDEO", "SETTING"), help="also a real clip, with its export (and the .json beside it)")
    ap.add_argument("--real-fusion-clip", action="store_true", help="put the real clip in a Fusion Clip (start set) too")
    ap.add_argument("--real-roi", default="", help="the region measured on the real clip, in the tracked file's px on the reference frame: x0,y0,x1,y1 (default: around the two points)")
    ap.add_argument("--measure-only", action="store_true", help="measure the renders already in target/resolve-proof (no Resolve)")
    ap.add_argument("--only", help="run only cases whose letter is in this string, e.g. ABR")
    ap.add_argument("--stay", action="store_true", help="stay in the scratch project afterwards")
    args = ap.parse_args()
    cases = synthetic_cases()
    if args.real:
        roi = tuple(float(v) for v in args.real_roi.split(",")) if args.real_roi else None
        cases.append(real_case(*args.real, False, roi))
        if args.real_fusion_clip:
            cases.append(real_case(*args.real, True, roi))
    if args.only:
        cases = [c for c in cases if c.name[0] in args.only]
    results = []

    def done(case, raw, stab, wired):
        print(f"-- {case.name}")
        results.append(evaluate(case, raw, stab, wired))
        preview(raw, stab, OUT / case.name.split()[0] / "preview.mp4", 960 if case.real else 640, case.points if case.real else ())
        (OUT / "report.json").write_text(json.dumps(results, indent=1))

    if args.measure_only:
        for case in cases:
            d = OUT / case.name.split()[0]
            done(case, next(d.glob("raw.*")), next(d.glob("stabilized.*")), None)
    else:
        r = connect()
        with Scratch(r, args.stay) as p:
            for case in cases:
                print(f"-- {case.name}: making it in Resolve")
                done(case, *make(r, p, case, OUT / case.name.split()[0]))
    print("\n| case | raw: moves up to / turns up to | stabilized: moves median / max | stabilized: turns median / max | paste wired itself |")
    print("|---|---|---|---|---|")
    for res in results:
        s, w = res["stabilized"], res["raw"]
        k = "anchor_moved_px" if "anchor_moved_px" in s else "drift_px"
        wired = {True: "yes", False: "no", None: "–"}[res["wiring_by_paste"]]
        print(f"| {res['case']} | {w[k]['max']:.1f} px / {w['rotation_deg']['max']:.2f}° | {s[k]['median']:.2f} / {s[k]['max']:.2f} px | "
              f"{s['rotation_deg']['median']:.3f}° / {s['rotation_deg']['max']:.3f}° | {wired} |")
    print(f"\nreport: {OUT / 'report.json'}; side-by-side previews in each case's folder")


if __name__ == "__main__":
    main()
