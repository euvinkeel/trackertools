# CoTrack editor

A local, editor-agnostic subject tracker built on CoTracker3. Drop a video in,
define subjects with one or more points, let the model track them in the
background, correct it by hand where it fails, and export positions for any
NLE (Resolve, Premiere, …).

## Run

```
.venv\Scripts\python.exe editor\server.py
```

Open http://127.0.0.1:8000. The model loads in the background (~10 s); you can
open a video meanwhile. Set `COTRACK_EDITOR_PORT` to change the port.

## Workflow

1. **Open a video** — drag & drop (copied into `editor/data/videos`) or paste a
   file path (used in place, no copy — best for very large files).
2. **Subjects** — `N` creates one and lets you name it; it becomes the selected
   subject. Subjects are followed by *trackers*: CoTracker points (`P#`) and
   template trackers (`T#`, below). Every click on the frame adds a point to
   the selected subject *on that frame*.
   A subject's center is **stateful**: it starts at the mean of the trackers
   that contribute on its first frame, and from then on it is pushed, frame by
   frame, by the mean of the trackers' motions (each delta since that tracker
   last contributed). So adding, removing or re-anchoring trackers never makes
   the subject jump, and a manual adjustment simply re-anchors where the
   pushing continues from. Trackers that are *not found* (template) or
   *drifted* (outside the bounds) don't push; hidden CoTracker points still do,
   and their visibility is reported separately. A frame where nothing
   contributes holds the position and is reported as unknown/lost. When a
   tracker comes back after a confirmed failure (drift, not-found, or a
   motion-consistency rejection), its motion reference resets so the rejected
   displacement is never injected into the subject.
3. **Template trackers** — for small, rigid patterns CoTracker handles poorly
   (a mouse cursor, a logo, a HUD icon). `Shift`+drag a box around the pattern
   to open the template editor, which shows the original frame's pixels
   magnified: paint the mask with the brush (right-click erases), try
   **Auto (from border)** to select the object the box encloses, and place the
   **hotspot**, the exact point to follow (e.g. the cursor tip). `Enter` saves,
   `Esc` cancels. Each frame is searched with OpenCV (masked normalized
   cross-correlation, sub-pixel), falling back to a full-frame search when the
   pattern is lost. Frames scoring below the tracker's **match threshold** are
   *not found* (dashed square marker, red on the timeline); the threshold can be
   changed at any time without re-tracking. When the pattern changes
   appearance, select the tracker and `Shift`+drag another box to add a
   **look**; the best-scoring look wins per frame. Looks get stable slots:
   removing one never changes the meaning of already-tracked frames, and
   results keep reporting which look produced them. Adding, editing or removing
   a look applies **from the cursor on**: frames before it were tracked with the
   previous looks and keep their results. A per-segment *write boundary* freezes
   that prefix, so even the subsequent `G` run (which warms up from a frame
   before the cursor) cannot overwrite it; undo brings back anything cleared.
   **Re-track from here** clears the tracker's results from the cursor on.
4. **Pattern library** — the template editor's **Save to library** keeps the
   pixels, mask and hotspot in browser storage under a name. Saved patterns
   appear in the sidebar's **Patterns** list and work in *any* video: **Use**
   adds the pattern as another look to the selected template tracker (copied
   pixels travel with the project, so later library edits don't change it), or
   opens the editor on the pattern to start a new tracker with an explicitly
   placed hotspot. **Export library** / **Import…** share patterns between
   browsers or machines as JSON. If browser storage is unavailable the list is
   labeled *temporary* (memory only); the library never silently deletes old
   patterns.
5. **Track** — `G` starts tracking from the cursor in the background; the
   Tracker panel shows a live preview and progress. `G` again halts. Nothing
   is tracked until you ask, so you can place and correct trackers freely.
   `Shift+G` tracks only the selected trackers (or the selected subject's).
   Select several by dragging a box on empty frame, `Ctrl`+click, or `A`.
   Confirmed automatic ends (below) retire the affected trackers inside the
   running job at their next window boundary; other trackers keep going.
6. **Correct**
   - *Drag* an auto-tracked point: re-anchor. Tracking restarts from the new
     spot on the next Go; earlier frames are untouched.
   - *Double-click* a tracker: it ends from this frame on (on its first frame it
     is deleted). Other trackers keep informing the subject.
   - `Ctrl`+drag a box: the selected subject's trackers **inside** it end from
     this frame on. `Ctrl`+`Alt`+drag: those **outside** it. One undo step.
   - `M`: toggle manual animation for the selected tracker(s) from this frame.
     While manual, dragging sets linear keyframes and the model is not used.
     Toggling off resumes auto tracking from the manual position.
   - **Off-frame**: trackers can be placed and animated outside the frame (zoom
     the viewer out). The model can't track off-screen, so a point clicked or
     dragged outside becomes manual; resume auto tracking (`M`) once it is
     back inside. A segment that would resume from off-frame is flagged
     "off-frame seed" and skipped by the tracker.
7. **Reversible ends** — `E` ends the selected tracker(s) from this frame on;
   `U` (or **Remove end** / **Remove all ends** in the sidebar) removes the end
   again. Ending *retains* later keyframes, manual ranges and stored results —
   clearing the end restores access to them, and any missing frames are
   computed by the next `G`. Ends can also be dragged on the timeline: click the
   marker to focus it, drag to move it, `Del`/`Backspace` removes it (the
   tracker itself stays). An end has a source: **user** (rose marker) or
   **auto** (orange `A` marker, with its reason and confirmation frame in the
   inspector). Removing or moving an automatic end turns automatic ending off
   for that tracker until **Automatic ending** is re-enabled, so it cannot
   immediately reappear.
8. **Offset** — drag the subject marker's ring (not its center) to shift the
   subject away from the position its trackers pushed it to, e.g. from a helmet
   to the rider's center. This keys the offset on the current frame; keys
   interpolate linearly and hold before the first and after the last, so the
   pushed position keeps moving underneath. A ghost marks the pushed position
   and a dashed line joins it to the final position. Keys are listed (and
   deleted) in the subject panel; `[` `]` jump between them when no tracker is
   selected.
9. **Finetune layers** (`R`) — additive, keyed corrections stacked on top of
   the offset: `final = pushed + offset + finetune`. Each layer has an
   enable checkbox, a weight and a stack position (listed in the subject panel).
   In `R` mode, drag anywhere on the frame: movement is **relative**
   (pointer-lock — there is no absolute handle), `Shift` is 1/10 speed,
   `Q` shows a targeting loupe, and `←` `→` step frames while holding the value
   (each stepped frame gets a key). Values hold before the first and after the
   last key. One drag = one undo step. **+ Layer**, **Clear layer** and delete
   live in the sidebar; weight 0 effectively disables a layer without losing it.
10. **Bounds** — an optional rough box per subject, animated over time, that
    keeps its trackers honest. It is drawn around the subject (solid for the
    selected subject, dashed for the others) and edited in the subject panel's
    Bounds section.
    - **Puppeteer pass** (`P`): click the subject; after a short countdown the
      video plays slowed down and you follow the subject with the mouse.
      Jiggle the mouse to make the box bigger (e.g. while the subject moves
      erratically); keep it still for a tight box. `Esc`, `Space`, a click or
      the end of the video stops the pass. The recording is smoothed, shifted
      earlier to make up for your hand's lag, widened to include nearby motion.
      **Preset** (Rough 1× / Fine 0.5× / Extra fine 0.25×), **Speed**, **Size**
      (jiggle gain), **Lag**, **Min half** and **Pad** are in the Bounds
      section. While recording, the committed composite is drawn faintly for
      comparison and the live box has no trail.
    - **Multiple passes** are kept independently. Each recorded pass has a
      refinement **level**: `P` records a *new refinement level* on top of the
      previous ones; **+ Take L…** records another attempt at the same level and
      the takes are averaged (robustly: a wild take cannot drag the center).
      Disagreement between takes adds a modest margin (never a shrink — a box
      is the intended region, not a confidence interval) and marks that range
      *uncertain* for automatic ending. Each pass has an enable checkbox, a
      weight slider, delete, and (for recorded passes) a **tune** section whose
      lag/gain/min-half/pad changes regenerate it from the original mouse
      samples. Coverage shows as thin lines in the subject's timeline row.
    - **Bounds mode** (`B`): on a frame without bounds, drag a box around the
      subject to fill that gap (the whole video if there are none yet). With
      bounds, drag inside the box to move it, its edges/corners to resize, or
      outside to redraw it. Manual edits form a correction layer that wins over
      the recorded passes where it is set; changes are full on the current
      frame and fade out smoothly over ±**Falloff** seconds (0.2 s by default;
      wheel while dragging or `Alt`+wheel adjusts it; the curve shows over the
      timeline ruler). `Esc` or `B` leaves the mode. **Clear from here** /
      **Clear all** remove the manual layer and trim/remove the passes.
    - **Drift**: a tracker outside its subject's effective box (plus 10% of the
      box size on each side) is flagged *drifted* (orange dashed marker with a
      cross, orange on the timeline) and left out of the subject's position.
      **End drifted here (n)** ends each drifted tracker from the frame where it
      left the box. Tracking resumes from a tracker's last on-bounds frame.
    - Template trackers only search inside the box, so a look-alike elsewhere in
      the frame can't be picked up. Editing bounds clears their results from the
      first changed frame (undo restores them) and halts a running track.
    - **Guide CoTracker points** (on by default): the box's motion is used as
      the points' starting guess on new frames. This only matters for extreme
      motion (well over 100 px per frame at 720p), so editing bounds keeps
      existing CoTracker results; the guide applies from their next run.
11. **Automatic ending** — per subject (sidebar → *Automatic ending*):
    - **End after a sustained escape**: once a tracker has been outside the
      effective bounds for `max(3 frames, Escape seconds)` (0.1 s by default),
      it gets an automatic end placed at the **first** escaped frame. A brief
      excursion produces nothing; hidden CoTracker points and manual animation
      are exempt; missing bounds or results break the confirmation run.
      A pass being recorded does not generate ends until it is committed.
    - **Motion consistency**: fit the translation + rotation + zoom that most
      trackers agree on (RANSAC over the pairs, robust thresholds; see
      `motion.js`) and compare each tracker's residual. This catches a straggler
      even when every dot has a different motion vector (dots spreading out or
      shrinking in as the camera zooms, rotating around a point, …). Options:
      *Off*, *Flag outliers* (inspector only), *Exclude from pushing*, or *End
      after sustained disagreement*. Fewer than four usable dots, a tight
      cluster, or a 4-vs-4 split returns *insufficient evidence* rather than a
      verdict; analysis compares both the adjacent frame and a ~0.25 s interval
      so slow drift is caught too. A moving arm disagreeing with a torso is a
      legitimate motion difference: keep motion handling off for flexible
      subjects and use bounds instead.
    - Analysis is deterministic and runs on stored observations, independently
      of where you scrub; automatic ends never hide the evidence they are based
      on. They are derived state (not undo steps) and appear in the inspector,
      exports and the timeline.
12. **Export** — JSON (full data, format below), CSV (subject positions),
    **Norm CSV** (0–1 coordinates, for NLE scripts), or an experimental Fusion
    `.setting` (see below).

Press `?` in the app for all shortcuts. `editor/PLAN.md` tracks what's done and
what's next.

## Export format (`cotrack.tracks`, version 3)

```jsonc
{
  "format": "cotrack.tracks",
  "version": 3,
  "video": { "name": "clip.mp4", "width": 1920, "height": 1080, "fps": 60, "frameCount": 36080, "durationSeconds": 601.3 },
  "coordinates": { "units": "source pixels", "origin": "top-left" },
  "visibilityThreshold": 0.6,
  "subjects": [{
    "id": 1, "name": "Hero", "color": "#22d3ee",
    "offsetKeys": [{ "frame": 120, "dx": 4.5, "dy": -12 }],
    "boundsGuide": true,              // bounds guide CoTracker's starting guess
    "driftPolicy": "flag",            // flag | end (automatic ending on bounds)
    "escapeSeconds": 0.1,             // sustained escape before an automatic end
    "motionPolicy": "off",            // off | flag | exclude | end
    "boundsManual": { "frames": [10, 599] },          // manual correction layer range, or null
    "boundsPasses": [{                // recorded Puppeteer passes (metadata)
      "id": 7, "name": "Fine 1", "level": 1, "kind": "refine", "enabled": true,
      "weight": 1, "range": [10, 599], "settings": { "rate": 0.5, "lag": 0.25, "gain": 1 },
      "samples": 1200                 // pointer samples retained for regeneration
    }],
    // one entry per frame on which the subject has at least one tracker
    "track": {
      "frame": [100, 101],
      "x": [816.9, 819.5],            // final position; null when unknown (not yet tracked)
      "y": [428.1, 429.7],
      "rawX": [812.4, 815.0],         // position the trackers pushed it to (before offset/finetune)
      "rawY": [440.1, 441.7],
      "offsetX": [4.5, 4.5], "offsetY": [-12, -12],
      "finetuneX": [0, 0], "finetuneY": [0, 0],   // x = rawX + offsetX + finetuneX
      "status": ["ok", "partial"],    // ok | partial (some trackers untracked) | unknown | lost (all not found or drifted)
      "trackers": [3, 3], "visibleTrackers": [3, 2], "hiddenTrackers": [0, 1], "pendingTrackers": [0, 0],
      "notFoundTrackers": [0, 0],     // template trackers below their threshold
      "driftedTrackers": [0, 1],      // trackers outside the bounds (+10% margin), not counted
      "outliers": [0, 0],             // confirmed motion-consistency outliers, not counted
      "bounds": [[815, 430, 120, 90], null]  // effective [cx, cy, w, h]; null = none
    },
    "trackers": [{
      "id": 4, "label": "P4", "kind": "point",       // point (CoTracker) | template
      "start": 100, "end": null,                     // user end (exclusive); null = until the end
      "autoEnd": { "f": 300, "reason": "bounds", "confirmedAt": 305 },  // or null
      "noAutoEnd": false,                            // user removed an automatic end
      "effectiveEnd": null,                          // earliest end, or null for the video end
      "keyframes": [{ "frame": 100, "x": 810, "y": 438, "kind": "create" }],  // create | anchor | manual
      "manualRanges": [{ "start": 200, "end": 260 }],
      // template trackers also have "threshold": 0.7, "looks" and "retiredLooks"
      //   "looks": [{ "id": 5, "slot": 0, "rev": 1, "f": 100, "x": 800, "y": 430, "w": 20, "h": 26,
      //               "hx": 2.5, "hy": 2.5, "mask": "<base64, w*h bytes, 1 = used; empty = whole box>",
      //               "tmpl": "<embedded PNG when the look came from the library, else null>" }]
      //   Results report both the stable slot and the current list index.
      "samples": {                                   // every frame of the tracker's lifespan
        "frame": [100, 101], "x": [810, 812.2], "y": [438, 439.0],
        "visibility": [1, 0.97],                     // model confidence 0..1; 1 for manual; null if untracked
        "score": [null, null], "look": [null, null], // template trackers: match score -1..1, current look index
        "lookSlot": [null, null],                    // stable slot that produced the frame
        "notFound": [false, false],                  // score below threshold (x/y = best candidate)
        "drifted": [false, false],                   // outside the subject's bounds (+10% margin)
        "mode": ["auto", "auto"],                    // auto | manual | pending | blocked | nodata
        "contributes": [true, true]                  // pushed the subject on this frame
      }
    }]
  }]
}
```

Version 1/2 readers keep working for positions: `x`/`y` are still the final
subject position. (v1 called trackers `points` and had no offset; v2 packed the
look's *array index*, v3 uses the stable slot.) Projects saved by older builds
load automatically — an old single bounds store becomes the base pass so newer
passes can refine it.

The CSV has one row per subject and frame: `frame, time_s, subject_id, subject,
status, x, y, raw_x, raw_y, offset_x, offset_y, finetune_x, finetune_y,
bounds_cx, bounds_cy, bounds_w, bounds_h, trackers, visible_trackers,
hidden_trackers, pending_trackers, not_found, drifted, outliers` (bounds columns
are empty where the subject has none). **Norm CSV** is
`subject, frame, time_s, x_norm, y_norm` with `0..1` coordinates.

Positions may lie outside `0..width` × `0..height` when a tracker (and so
possibly the subject) is animated off-frame.

(0,0) is the top-left corner of the top-left pixel. Divide by width/height for
normalized coordinates (e.g. Fusion uses 0..1 with the origin at the bottom-left,
so `y_fusion = 1 - y / height`).

### DaVinci Resolve / Fusion (experimental)

The **Fusion** button downloads a `.setting` Lua table containing one `Tracker`
node per subject, with the subject's path as `Pattern` keyframes (Fusion
coordinates: 0..1, origin bottom-left). The file is generated to Fusion's
`.setting` schema but has **not been validated inside Resolve** — open it in
Fusion and check the node before relying on it. The Norm CSV is the safer
interchange for scripts and expressions; Premiere has no standard keyframe
interchange, so use the CSV/JSON there.

## How it works

- `engine.py` drives CoTracker3's online model with a rolling window, so memory
  stays flat for any video length and points can join/leave mid-run. The
  per-window step runs from captured CUDA graphs (~160 fps on 1080p60 with an
  RTX 4090). Results match the stock predictor to within 0.003 px.
- `templates.py` tracks template trackers on the CPU with OpenCV in its own
  thread (hundreds of fps at 1080p), alongside the GPU CoTracker worker; both
  report as one run (`runs.py`). Looks are cut from the original video, not
  the proxy (`frames.py`); library looks carry their own PNG pixels, so the
  original video isn't needed to track them.
- Subject bounds travel with each run (`bounds.py`): template trackers search
  only inside them; CoTracker points get the box's translation and scale as
  their starting guess on new frames (`engine.py`). The effective box is the
  manual correction layer over a composite of the recorded passes
  (`bounds-stack.js`): takes average within a level, finer levels blend over
  coarser ones inside their own coverage with a 0.2 s endpoint fade. Drift
  flags are computed in the browser at display time, so they never need
  re-tracking.
- The subject's position is stateful (`project.js`): a per-frame integration of
  the trackers' motions, cached in 256-frame checkpoints. Every edit records
  the first frame it can affect, so a keyframe drag or a streaming tracking
  result only recomputes the frames from there on (a drag at the playhead costs
  ~0.1 ms on a 10-minute clip).
- Quality analysis (`quality.js`, `motion.js`) is pure and deterministic: it
  fits the motion the cohort agrees on and scans raw observations for sustained
  escapes/disagreement, producing reversible automatic ends. A running job is
  told to retire those segments at their next window boundary.
- `proxy.py` builds a 720p, short-keyframe-interval proxy for display when a
  video has sparse keyframes (typical for recordings) or a codec browsers can't
  play, so frame stepping stays instant. Tracking always reads the original.
  Proxies live in `editor/data/proxies` (roughly 2–4 GB per hour of footage) and
  can be deleted at any time. `COTRACK_PROXY_HEIGHT` changes their resolution.
- Projects autosave to `editor/data/projects/<video-id>.json`, including the
  pattern pixels used by library looks, dormant results past an end, and the
  write boundaries that protect corrected prefixes.

### CoTracker crop experiment

`tests/bench_crop.py` measures full-frame tracking against a moving crop that
follows the subject's rough bounds (3× the box, min 160 px, model aspect) with
the engine's rolling overlap chaining. On a textured 56 px card over panning
1080p60 footage, the crop roughly halves the error (median 0.7–1.6 px vs
1.6–2.6 px full-frame). It is **not exposed** as an option yet: the crop depends
on a rough bounds pass being present and trades away global recovery (hidden
frames, teleports). A production crop mode would need a stabilized crop plus a
global fallback. The script also documents the failure case: a single CoTracker
point on a small uniform sprite (the synthetic cursor) is not trackable at
either resolution — use a template tracker for those.

## Tests

From `editor/`: `npm test` (Node, no install needed) covers the model, template
maths and stable look slots, results write boundaries, bounds/Puppeteer
synthesis, the pass composite, the stateful position (including that its
incremental cache agrees with a from-scratch integration after every kind of
edit), motion-consistency fitting, automatic-end analysis and the pattern
library. With the repo venv:
`..\.venv\Scripts\python.exe tests\test_template_bounds.py` checks template
search inside bounds, `tests\test_stored_pattern.py` checks library pixels
(no video decode, invalid pixels are an error), `tests\bench_bounds_guide.py
[speed]` measures the CoTracker starting guess on a synthetic fast clip,
`tests\bench_crop.py` runs the crop experiment, and `tests\bench_base.mjs`
(Node) times the position integration. `tests\ui_*.py` are Playwright browser
tests against a running editor (clean editor + the synthetic cursor clip in
`%LOCALAPPDATA%\Temp\opencode`): `ui_templates.py`, `ui_bounds.py`,
`ui_prefix.py` (look correction + Go keeps the prefix), `ui_finetune.py`
(finetune layers, automatic ends, end-marker editing), `ui_library.py`
(pattern library, bounds passes, reversible ends) and `ui_retire.py` (live
retirement stops only the named segments while the rest finish).

## Limits

- The model tracks at 512×384 internally; positions are scaled to source pixels.
  Expect sub-pixel noise on 4K footage.
- Tracking runs forward from each tracker's creation frame (no backward tracking yet).
- CoTracker3 is licensed CC-BY-NC 4.0 (non-commercial).
