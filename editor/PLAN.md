# CoTrack editor v2 — guided tracking plan

Status: implementation in progress, 2026-09-23. Builds on v1 (points, manual
ranges, streaming CoTracker3 engine, JSON/CSV export, off-frame positions,
subset tracking). Latest checkpoint: §0 (reversible ends, pass stack, automatic
ending, motion consistency, finetune, pattern library, crop measurement).

### Checkpoint — §0 (endings, passes, quality, finetune, library), complete

This build implements the handoff plan in order:

1. **Template correctness + reversible ends.**
   - `Project.updateLook` referenced an undefined `p` — fixed.
   - Looks now have stable, never-reused **slots** (`slot`, `nextLookSlot`,
     `retiredLooks`); packed results carry the slot, so removing a look no
     longer shifts the meaning of preserved results. Old projects migrate to
     slot = array index, keeping their packed values valid. The server packs
     `spec.slot` (`templates.py`).
   - `ResultStore` has per-key **write boundaries**: truncating from f freezes
     frames before f, so the next `G` (which warms up from before the cursor)
     cannot overwrite the corrected prefix. Boundaries persist in saves and are
     restored by undo.
   - `ui_prefix.py` 15/15: track 600 frames → add a look at 400 → `G` →
     frames 0 and 399 byte-identical, boundary 400 persisted, undo/redo.
2. **Reversible tracker endings.** `end` (user) and `autoEnd` (automatic, with
   `reason`/`confirmedAt`) are separate; the effective end is the earliest.
   Ending retains keys, manual ranges and results; `clearEnd`/`U` restores them.
   Removing or moving an automatic end sets `noAutoEnd` until **Automatic
   ending** is re-enabled. UI: lifespan source, Remove end / Remove all ends,
   draggable timeline markers (auto = orange `A`), focus + `Del`/`Backspace`,
   inspector provenance, export fields. `Project.segments(p, end)` takes the
   lifespan so quality analysis can read raw observations past an auto end.
3. **Stored Puppeteer passes + composite bounds.** `bounds-stack.js`: takes
   average within a level (weighted median centers for ≥3, log-space sizes),
   disagreement adds a margin and marks *uncertain*; levels blend coarse→fine
   inside their coverage with a 0.2 s endpoint fade. `Project.boundsPasses`
   (each with its own ChunkStore, samples and settings), `boundsAt` = manual
   layer over the composite, `boundsExtent`, `packBoundsRange`, `trimBoundsPasses`.
   Puppeteer records passes (`P` = new refinement level, **+ Take L…** averages
   into the current level), with capture presets (Rough/Fine/Extra fine), live
   box + faint committed composite, per-pass enable/weight/delete/tune
   (regenerating from the retained samples), timeline coverage lines and
   undoable invalidation. Old single-store projects migrate to a base pass.
4. **Automatic endings + live retirement.** `quality.js` scans raw states for
   sustained escapes (`max(3 frames, escape seconds)`, end placed at the first
   escaped frame) and, optionally, sustained motion disagreement. Derived state
   (no undo entries), recomputed on results/bounds/policy changes, reported in
   the inspector and exports. A running job receives `{type:"retire"}` and stops
   the affected segments at the next window boundary (`engine.py`,
   `templates.py`, `runs.py`, `server.py`, `tracker.js`).
5. **Motion consistency.** `motion.js` fits translation + rotation + uniform
   zoom by RANSAC over point pairs (deterministic, no server round-trip),
   measures residuals against an absolute floor and robust inlier sigma, and
   returns *insufficient evidence* for small/tight/split cohorts. `quality.js`
   compares adjacent frames and a ~0.25 s interval; policies: off / flag /
   exclude from pushing / end. Excluded or drifted trackers reset their motion
   reference on return, so rejected displacement is never injected.
6. **Finetune layers (Phase D).** `finetune.js` mode `R`: relative pointer-lock
   nudging of the active layer, Shift 1/10, `Q` loupe, `←`/`→` step-and-hold,
   one undo step per session; sidebar layer list with enable/weight/stack/delete/
   clear; `final = pushed + offset + finetune` unchanged.
7. **Pattern library (completed).** Popup **Save to library** (name, updates the
   pattern it was opened from), sidebar **Patterns** list with thumbnails, Use
   (adds a look to the selected template tracker, or opens the editor on the
   pattern for a new tracker with explicit hotspot), delete, export/import JSON,
   persistent/temporary labeling, no silent eviction. Library looks embed their
   pixels (`tmpl`) so they work without the source video; the editor loads them
   from the stored PNG and copies edited pixels into the look. Backend uses
   stored pixels without decoding the video and **errors** on invalid pixels
   instead of silently matching video content.
8. **Exports/polish.** Export format v3 (end provenance, `noAutoEnd`, effective
   end, `outliers`, `lookSlot`, pass metadata, manual layer range), CSV gains
   `outliers`, new **Norm CSV** (0–1) and an experimental Resolve/Fusion
   `.setting`; help overlay and README updated.

Tests: `npm test` 8 suites (model, templates+slots, bounds, bounds stack,
position/cache, motion, quality, patterns); Python `test_template_bounds.py`,
`test_stored_pattern.py`; browser `ui_prefix.py` 15/15, `ui_finetune.py` 24/24,
`ui_bounds.py` 30/30, `ui_library.py` 24/24, `ui_retire.py` 5/5,
`ui_templates.py`, `ui_test`, `ui_test2`, Phase A 22/22 — all with 0 console
errors.

Crop experiment (Phase E): `tests/bench_crop.py` compares full-frame vs a
moving crop with rolling overlap chaining. On a textured 56 px card the crop
roughly halves the error (median 0.7–1.6 vs 1.6–2.6 px); not exposed because it
depends on rough bounds and loses global recovery. Remaining Phase F: validate
the Fusion `.setting` inside Resolve, Premiere recipe polish.

### Checkpoint — Phase B frontend

Template creation via Shift+drag, the original-frame mask/hotspot editor,
multiple looks, threshold, re-track, distinct found/not-found displays, and
score/look inspection and export are wired up. The browser flow on the synthetic
cursor clip passes through frame 599, including the hidden interval (305–329),
with zero console errors (`editor/tests/ui_templates.py`). The template model
test passes (`editor/tests/test_templates.mjs`). A WebSocket serialization bug
with NumPy coordinates found by this test was fixed in `editor/templates.py`.

Phase B is complete: the older browser regressions pass again (`ui_test`,
`ui_test2`, Phase A 22/22, all with 0 console errors). The Phase A test now
checks that the template popup opens and closes. Popup polish: the canvas is
backed at display resolution (nearest-neighbour pixels), the hotspot has a
high-contrast cross, and Shift places the hotspot freely. Subjects whose
trackers are all not found read "not found on this frame". The README
documents template trackers and the new export fields.

### Checkpoint — Phase C (bounds), complete

Built: `bounds.js` (falloff nudge, gap fill, clear, pass merge, Puppeteer
synthesis), per-subject bounds store, drift flags (excluded from the mean,
drift-aware resume), End drifted, Bounds mode `B`, Puppeteer mode `P`
(`puppeteer.js`), sidebar Bounds section (speed/size/lag/falloff, guide
toggle), timeline bounds strip + drifted state + falloff curve, inspector and
export fields (JSON `bounds`, `driftedTrackers`, `drifted`; CSV `bounds_*`,
`drifted`), undoable result invalidation (`editBounds`, result patches,
`tracker.abandon()`), backend `bounds.py`, template search restricted to
bounds, CoTracker starting guess in `run_window`.

Tests: `test_bounds.mjs` (synthesis: lag, jiggle→size, inclusiveness; model)
and `test_templates.mjs` pass; `test_template_bounds.py` (same accuracy with
bounds, 0.017 px median, ~1.5× faster; wrong bounds → not found);
`ui_bounds.py` 23/23 with 0 console errors (includes a scripted Puppeteer pass
following the true path with a human-like lag: cursor inside the box on 66/66
frames).

Starting-guess measurement (`bench_bounds_guide.py`, textured patch at 720p):
no difference up to ~100 px/frame (CoTracker converges either way); at
~140 px/frame samples >16 px off drop 1.7% → 0.8%, at ~200 px/frame 23% → 11%.
Decision: bounds edits invalidate template trackers only; the guide (default
on) applies to CoTracker's next run instead of discarding its results.

Finish-up: older browser regressions re-run after Phase C and all pass
(Phase A 22/22, `ui_test`, `ui_test2`, `ui_templates`, 0 console errors);
README documents bounds, Puppeteer, drift, the new export fields and how to run
the tests (`npm test` in `editor/`); screenshots checked; the subject timeline
row now shows drifted-only frames in orange instead of "not found" crimson.

### Checkpoint — stateful position, live pass box, shorter falloff

- **The subject's position is now stateful** (§2): it starts at the mean of the
  trackers that contribute on its first frame and is then pushed by the mean of
  their motions (each delta since that tracker last contributed). Removing or
  re-anchoring trackers no longer jumps the subject to a new absolute mean; a
  manual adjustment is an offset key that the pushing continues from; a frame
  where nothing contributes holds the position and the motion during the gap is
  applied when trackers return. `project.js` (`baseAt`/`_baseExtend`), with a
  cached integration: `touch(from)`/`ResultStore._note(from)` record the first
  frame an edit can affect, and the cache resumes from a checkpoint (every 256
  frames) just before it instead of integrating from the start. Measured on a
  10-minute 60 fps clip with 8 trackers: full integration 14 ms, a drag at the
  playhead 0.07 ms, a results write behind the playhead 0.17 ms.
- **The Puppeteer pass draws the box it is writing live** under the hand
  (synthesised from the samples so far) instead of a growing trail.
- **Bounds falloff default 0.2 s** (was 1 s), stored under a new key so the new
  default applies; the wheel/Alt+wheel adjustment is unchanged.
- Tests: `test_position.mjs` (the model: removal, late joiners, gaps, manual
  re-anchoring, hidden points, plus a cache-vs-fresh-integration equivalence
  check over 15 edit scenarios), `test_project.mjs` moved into the repo and
  updated, `bench_base.mjs`; `ui_bounds.py` now 27/27 (push model, live box,
  falloff default).

### Checkpoint — look changes from the cursor; pattern library, complete

Done and tested:

- **A template look change no longer wipes the tracker's history.** The result
  key used to carry a look-set signature (`id@seed@x,y#4.1-9.2`), so adding a
  look re-keyed every segment and orphaned all earlier results. Keys are now
  independent of the look set (`project.js`), and adding, editing or removing a
  look (or applying a library pattern) truncates results **from the cursor on**
  through `app.editTemplateLook(label, tids, f, fn)`, which is one undo step and
  restores the cleared results on undo. `ResultStore.load` folds old signed keys
  into the bare key (longest run wins) so existing projects keep their progress.
  Covered by `test_project.mjs`, `test_templates.mjs` (the key survives a look
  change) and `ui_templates.py` (track a cursor, add a look at frame 400 →
  frames 0–399 stay, 400+ cleared, undo restores 599). §0 adds stable look slots
  and per-key write boundaries on top (see `ui_prefix.py`).
- **Pattern library.** `patterns.js` keeps saved patterns in localStorage
  (`cotrack.patterns.v1`; name, box, hotspot, base64 PNG of the pixels, mask;
  200 patterns / 4 MB cap, in-memory fallback so it is testable in Node).
  `templates.py` accepts a look's own `tmpl` (base64 PNG) and uses it instead of
  cutting pixels from the video, without decoding the video at all; invalid
  pixels raise instead of falling back. §0 wired the UI (Save to library,
  Patterns list, Use, export/import) and the tests
  (`test_patterns.mjs`, `test_stored_pattern.py`, `ui_smoke_new.py`).

Open for the user: a manual feel check of the Puppeteer passes (presets, lag,
take averaging), finetune nudging, and the automatic-ending thresholds; the
Fusion `.setting` export needs validation inside Resolve. All changes remain
uncommitted.

## 1. Goals

- **Rough in, precise out.** A human roughly guides (Puppeteer pass, nudges),
  trackers do the precise work, and a human can finish precisely (offset,
  finetune). Every layer is non-destructive and exported.
- **Many strategies, one subject.** CoTracker points and template trackers
  both contribute to a subject's position.
- **Everything stays editable.** Edits invalidate only what they affect, and
  everything is undoable.

## 2. Concepts and the position stack

```
Subject
├─ Bounds (optional)   animated box (cx, cy, w, h per frame); guides/restricts trackers, flags drift
├─ Trackers            CoTracker points (P#) and template trackers (T#): per-frame x, y, confidence
├─ Offset              keyed, linear; auto-keyed by dragging the subject marker
└─ Finetune layers     additive per-frame corrections recorded in Finetune mode; toggle + weight each

pushed(f) = pushed(f-1) + mean over contributing trackers of (pos(f) - pos(last contributing frame))
final(f)  = pushed(f) + offset(f) + Σ enabled layers: weight · delta(f)
```

The subject's center is **stateful**, not a per-frame average of absolute
positions: it starts at the mean of the trackers that contribute on its first
frame, and from then on it is pushed by the mean of the trackers' motions (each
delta measured since that tracker last contributed). Consequences:

- Adding, removing or re-anchoring trackers never makes the subject jump to a
  new absolute mean; the remaining trackers simply keep pushing it.
- A tracker joining later starts pushing from its second frame (its first
  position only anchors when nothing else is left, e.g. all trackers were
  replaced).
- A frame where nothing contributes holds the position. A gap that is only
  *pending* (results not computed yet) catches up at once when the results
  arrive; a tracker returning after a **confirmed failure** (drifted, not
  found, or a motion-consistency rejection) resets its motion reference
  instead, so the rejected displacement is never injected into the subject.
- A manual adjustment (an offset key) is absolute at that frame; the pushing
  continues from there, and the offset holds until another key.

| Tracker state at frame f | Contributes | Why |
|---|---|---|
| CoTracker auto, visible | yes | |
| CoTracker auto, hidden (occluded) | yes | the model's guess is still useful (v1 decision) |
| Manual (keyframed, incl. off-frame) | yes | |
| Template found (score ≥ threshold) | yes | |
| Template not found | no | its position is only the last known one |
| Drifted (outside bounds) | no | flagged; likely off-track |
| Pending / blocked / no data | no | |

If nothing contributes, the subject has no position on that frame, and offset
and finetune don't apply (the pushed position itself is held for later frames).
The inspector shows every tracker's state, score and flags regardless.

## 3. Interaction

### Viewer gestures (normal mode)

| Gesture | Action |
|---|---|
| Click empty frame | Add a CoTracker point to the selected subject (outside the frame = manual point) |
| Drag on empty frame | Box-select trackers (replaces the selection; Ctrl+click adjusts it afterwards) |
| Click / drag tracker | Select / re-anchor (auto) or keyframe (manual); off-frame drag switches to manual |
| Ctrl+click tracker | Toggle it in the selection |
| **Shift+drag** | Draw a template box → template editor (adds a look if a template tracker is selected) |
| **Ctrl+drag** | End the selected subject's trackers *inside* the box from this frame on |
| **Ctrl+Alt+drag** | End the selected subject's trackers *outside* the box from this frame on |
| Double-click tracker | End it from this frame on (deletes on its first frame) |
| Drag subject marker ring | Set an offset key on this frame (auto-key) |
| Wheel / middle-right-Space drag | Zoom / pan |

"End from this frame on" keeps earlier (good) data. On a tracker's first frame
it deletes the tracker. Box deletes are one undo step.

### Modes

Only one mode is active at a time. `Esc` (or the mode's key again) returns to
Normal.

| Mode | Key | Purpose |
|---|---|---|
| Normal | — | Trackers, selection, offset |
| Bounds | `B` | Edit the selected subject's bounds with smooth falloff (default 0.2 s) |
| Puppeteer | `P` | Record bounds by following the subject with the mouse during playback |
| Finetune | `R` | Record an additive correction frame by frame in a magnified loupe |

### Sidebar

- **Subject panel** (subject selected, no tracker):
  - Bounds: range, Puppeteer, Edit, Clear, speed, falloff.
  - Offset: value on this frame, delete key, clear.
  - Finetune layers: list with enable, weight, delete, new, active layer (rename is not implemented).
  - "End drifted here (n)".
- **Tracker panel:**
  - Kind (P point / T template), state, position and score on this frame, lifespan, keys.
  - Template trackers also get: looks as thumbnails (edit mask, remove) and a threshold slider.
- **Multi-selection:** the bulk panel as in v1.

### Timeline

- Subject row: a bounds coverage strip and offset key diamonds.
- Finetune layer coverage rows are not drawn yet (only bounds-pass coverage).
- Tracker rows gain two states: drifted (red tint) and not found (template).
- While nudging bounds, the falloff curve is drawn over the ruler.

## 4. Data model (project format v3; see §0 and README)

```jsonc
{
  "subjects": [{
    "id": 1, "name": "Cursor", "color": "#22d3ee", "hidden": false,
    "offset": [{ "f": 120, "dx": 4.5, "dy": -12.0 }],          // linear keys, hold outside
    "bounds": "<chunk store: cx, cy, w, h per frame; NaN = undefined>",
    "boundsGuide": true,                                         // use bounds as CoTracker starting guess
    "layers": [{ "id": 7, "name": "Finetune 1", "enabled": true, "weight": 1.0,
                 "keys": "<chunk store: dx, dy per keyed frame; NaN = no key>" }],
    "activeLayer": 7
  }],
  "trackers": [
    { "id": 2, "subjectId": 1, "kind": "point", "start": 0, "end": null,
      "keys": [{ "f": 0, "x": 812.5, "y": 440.0 }], "manual": [] },
    { "id": 3, "subjectId": 1, "kind": "template", "start": 0, "end": null,
      "keys": [{ "f": 0, "x": 960.0, "y": 540.0 }], "manual": [],
      "threshold": 0.7,
      "looks": [{ "id": 4, "rev": 1, "f": 0, "x": 955, "y": 536, "w": 18, "h": 26,
                  "hx": 5.0, "hy": 4.0, "mask": "<base64 w*h bytes, 0/1>" }] }
  ],
  "nextId": 8
}
```

- **Chunk stores** (bounds, finetune data):
  - Float32 arrays in 256-frame chunks, with NaN meaning "undefined".
  - Chunks are copy-on-write: an edit clones only the chunks it touches. Undo snapshots then share every unchanged chunk, so a nudge covering 120 frames costs about 2 chunks rather than the whole video.
  - Saved as base64 per chunk, like results.
  - A full-length hour at 60 fps is about 3.5 MB per subject.
- **Migration:** v1 files load as v2. `points` become trackers with `kind: "point"`, and there are no bounds, offset or layers.
- **Names:** point and template trackers share one id sequence, so labels are unique (P2, T3).

## 5. Undo, invalidation, results

- **Snapshots:** a snapshot is the JSON of the small structures plus a map of chunk references. "Did the edit change anything?" compares both.
- **Segment keys:**
  - Points: unchanged from v1 (seed-keyed; editing a seed invalidates only its segment).
  - Templates: the key is independent of the look set. Adding, editing or
    removing a look applies **from the cursor on** (the app truncates results
    there, undoably): the frames before it were tracked with the previous look
    set and stay valid. Older projects whose keys carried a looks signature
    (`#4.1-9.2`) are folded into the bare key when they load.
- **Bounds edits:**
  - What's invalidated: results of that subject's auto trackers, from the first changed frame to the end of each segment. Tracking is sequential, so later frames depend on earlier ones.
  - Undo: the removed result data is stored in the undo item, so undo restores it.
  - CoTracker points are included because bounds change their starting guess. If that proves annoying, we limit invalidation to template trackers.
- **Template results:**
  - They use the same `[x, y, v]` triplets as points.
  - `v` packs the match score and the winning look: `v = look·4 + (score + 1)`, with score in [−1, 1]. It's decoded in one helper.
  - The threshold is applied at display time, so moving the slider relabels found/not-found instantly. Search decisions used the old threshold; "Re-track from here" reruns them.
- **New action "Re-track from here":** truncates results for the selected trackers from the cursor. Go then resumes from their last good frame before it.

## 6. Bounds

### 6.1 Puppeteer pass

1. **Arm and start:** press `P` (or the sidebar button) and click the subject.
2. **Countdown:** 0.5 s, shown as a shrinking ring at the mouse.
3. **Record:** playback runs at the chosen speed (default 0.5×; 0.25/0.5/0.7/1×) while you follow the subject.
   The box being recorded is drawn live under the hand (synthesised from the
   samples so far), so the trail of the pass never clutters the picture.
4. **Stop:** Esc, Space, a click, or the end of the video.

Recording:

- **Pointer samples:** full rate via `getCoalescedEvents`, listened for on `window` so the mouse may leave the viewer (off-frame is fine).
- **One sample per presented video frame:** the last pointer position. A still mouse still produces data.
- **Time mapping:** `requestVideoFrameCallback` gives (now, mediaTime) pairs, which map pointer timestamps onto video time.

Synthesis is a pure function, unit-tested in Node. Windows are in real seconds, then multiplied by the playback rate to get video seconds:

1. **Lag compensation:** shift samples earlier by `lag` (default 0.25 s real).
2. **Center:** Gaussian-smoothed position (σ 0.12 s).
3. **Size:** the detrended spread, i.e. per-axis RMS of (sample − center) over σ 0.25 s. Half-size = `gain`·2.2·RMS + pad, with a minimum. Fine jiggle gives a tight box; wild motion gives a big one, up to the full frame.
4. **Inclusive of motion:** each frame's box is the union of boxes over [f − 0.1 s, f + 0.3 s]. It's biased forward because the hand lags.
5. **Final smoothing:** position σ 0.08 s, size σ 0.2 s.
6. **Merge:** replaces bounds over the pass range, cross-fading 0.2 s at both ends if bounds already existed there.

Settings (speed, lag, size gain) persist in localStorage.

### 6.2 Editing (Bounds mode)

- **Drag the body** to move, **edges/corners** to resize.
  - The change applies fully on the current frame and falls off with a raised cosine over ±R frames.
  - R defaults to 1 s. The wheel changes it during a drag, and the timeline shows the curve.
  - Only frames that have bounds are modified.
- **Drawing a box** on a frame without bounds fills that undefined gap with a constant box. With no bounds at all, it fills the whole video.
- **Clear:** all, or from this frame on.
- **Selected subject's view:** its box and center path (±falloff) are drawn. Other subjects' boxes are faint.

### 6.3 What bounds do

- **Template search region:** intersected with the local search window, or the whole bounds when the template is lost.
- **Drift flag:**
  - A tracker outside its subject's bounds (+10 % margin) is marked drifted and left out of the average.
  - "End drifted here" ends each drifted tracker at the start of its current drifted run.
- **CoTracker starting guess** (`boundsGuide`, default on):
  - Frames entering the model's window are normally initialized by *copying* the last known position.
  - Instead, the last position is moved along with the box (its translation and scale).
  - This helps fast motion without retraining.
- **CoTracker crop mode:** an experiment, see §8.

## 7. Template trackers

### 7.1 Creation and looks

- **Shift+drag** draws a box, clamped to the frame, and opens the **template editor** popup:
  - Shows an original-resolution crop from the server, magnified with nearest-neighbour.
  - Tools: Brush / Eraser with a size, Fill, Clear, and a Hotspot tool. The hotspot is the tracker's position, e.g. the cursor tip.
  - An empty mask on save means the whole box.
  - Enter saves, Esc cancels.
- **Adding looks:**
  - If a template tracker is selected, the popup defaults to "Add look to T#", with a "New tracker" switch.
  - A new look's hotspot defaults to the tracker's position on that frame, so all looks agree on where the tracker is.
- **Tracker panel:** look thumbnails (click to edit the mask, × to remove; at least one must remain) and a threshold slider (default 0.7).

### 7.2 Matching (engine, CPU, OpenCV)

Per frame, per active template tracker:

1. **Predict** the hotspot as last position + velocity (when it was found on the last frame).
2. **Local search:**
   - Window radius r = clamp(48 + 2·|v|, 48, 400) px around the prediction, intersected with the bounds (expanded by the template size).
   - Every look is tried with `TM_CCOEFF_NORMED` on grayscale, masked (unmasked when the mask is full). The best score wins.
3. **If the best score is below the threshold:**
   - Search the whole bounds, or the whole frame at half resolution if there are no bounds. The coarse pass is unmasked and keeps the top 3 candidates.
   - Refine each candidate masked, ±6 px at full resolution.
4. **Result:**
   - Found: sub-pixel peak (parabola fit), and the velocity is updated.
   - Not found: keep the last position, set velocity to 0, and still emit the score (for debugging).

Measured on this machine (OpenCV 5.0 CPU build, 1080p):

| Search | Time |
|---|---|
| Full frame, gray | 22–39 ms |
| Full frame, color | 68–120 ms |
| Full frame, masked | 70–585 ms |
| 200×200 window | 0.6 ms |

Hence local-first, with a coarse global search only when lost.

Limits:

- The pattern must keep the same size and orientation.
- Motion blur hurts.
- Look-alikes are kept out by the local window and the bounds.

## 8. CoTracker crop mode (experiment, Phase E)

- **Idea:** feed CoTracker only the subject's bounds as a moving, zoomed virtual camera. Small subjects get bigger, and motion inside a box that follows the subject is small. Both are CoTracker's weak spots in gameplay.
- **Design:**
  - Crops keep a fixed 4:3 aspect (the box is expanded, padded off-frame) and are resampled to 512×384 from the full-resolution frame on the GPU (`grid_sample`).
  - Crop motion gets extra smoothing.
  - Results map back through each frame's crop transform.
  - One model stream per subject in crop mode, batched as B>1 where possible.
- **Cost:** about one extra encoder and window pass per crop-mode subject.
- **Risks:** crop shake looks like camera shake to the model, and points leaving the box are lost.
- **Go/no-go:** error in pixels against hand-corrected tracks (made in the editor) on fast/small clips must be clearly lower than full-frame, with no regressions on normal clips. Only then does it become a per-subject option.

## 9. Offset

- **Keys:** `{f, dx, dy}`, linear between keys, held before the first and after the last. No keys means zero.
- **Dragging:** drag the subject marker's ring (7–15 px from its center; trackers win within 6 px). This auto-keys the current frame with dx = drop − mean.
- **Drawing:** when the offset isn't zero (always while dragging), a small ghost at the raw mean and a dashed line to the final marker.
- **Editing:** sidebar value, delete key, clear; diamonds on the subject's timeline row.

## 10. Finetune layers

A finetune layer is an additive, keyed correction on top of the offset. It
behaves like any auto-keyed animation: values interpolate between keys and hold
after the last one (and before the first) until another key overrides them.
Layers can be toggled and weighted, and they stack.

- **Enter:** `R` with a subject selected. It uses the subject's active layer, creating "Finetune 1" if there is none. Playback pauses.
- **Loupe:**
  - A magnified window over the viewer. Defaults: 360×360 px, 6× zoom. The wheel zooms 2–20×, Alt+wheel resizes, and both are also in the sidebar. They persist.
  - It is centered on the **base** position: everything except the active layer (trackers' mean + offset + other enabled layers). The view is therefore stabilized on the track.
  - It shows: the base crosshair at its center, the final marker, and a short trail of recent final positions in stabilized coordinates.
- **Two input styles**, switched with `Q` at any time:
  - **Nudge (default):**
    - Uses pointer lock. Mouse movement adds to the correction: Δ = movement / zoom × gain (Shift = 0.2× gain, for very fine work).
    - Holding still keeps the correction. You never run out of room.
    - The browser's Esc (which releases pointer lock) exits Finetune.
  - **Target:**
    - Normal cursor inside the steady loupe. Where you point *is* the final position: correction = (mouse − loupe center) / zoom.
    - The mouse outside the loupe writes nothing.
- **Stepping:**
  - `←`/`→` step one frame (`Shift` 10).
  - Every visited frame is keyed with the current correction on arrival, and updated while you move. What you saw is what you get.
  - Frames jumped over interpolate. Key auto-repeat works as slow manual playback.
- **Session boundaries:**
  - At the first write, a guard key is placed on the frame before the entry frame, holding the layer's previous value there. The session never changes earlier frames.
  - After the last visited frame the correction holds (normal keyed-animation behaviour) until a later key, if any, takes over.
- **Exit:** `R` or `Esc` commits the session as one undo step ("Finetune F1, frames a–b"). "Discard session" throws it away.
- **Layers:**
  - Stored as a chunk store of keys (dx, dy per frame; NaN = no key). Per-chunk counts make finding the previous and next key fast.
  - Layers stack additively, each with enable, weight (0–1), rename and delete.
- **No base position on a frame:** the loupe says so and nothing is written.
- **Picture source:** original-resolution crops from the server (§11.3), prefetched a few frames ahead. The displayed video frame (possibly the proxy) is drawn magnified as a placeholder until the crop arrives. On forward steps a warm decoder normally makes crops arrive within a few ms.

## 11. Architecture

### 11.1 Frontend (vanilla ES modules, no build)

New modules sit alongside the existing ones. The model code stays DOM-free so it can be unit-tested in Node.

| Module | Responsibility |
|---|---|
| `project.js` | Subjects, trackers (both kinds), keys, manual ranges, segments, tracker/subject state, the stateful pushed position (checkpointed cache, edit log), `planRun` |
| `chunks.js` | Copy-on-write chunk store (Float32 per-frame channels, NaN = undefined), serialize/restore |
| `bounds.js` | Pure functions: sample at f, gap fill, falloff nudge, pass merge, puppeteer synthesis |
| `finetune.js` | Layer evaluation, session bake (interpolate skipped frames), merge |
| `templates.js` | Score/look packing, mask encode/decode, border auto-select |
| `modes.js` | Viewer interaction modes (Normal, Bounds, Puppeteer, Finetune) behind one interface |
| `template-editor.js` | Magnified mask/hotspot popup |
| `loupe.js` | Finetune loupe rendering, crop fetching and prefetch |
| `puppeteer.js` | Recording (pointer and rVFC samples), playback control, hand-off to `bounds.js` |

**Viewer modes:**

- Every mode implements `enter`, `exit`, `onDown`, `onMove`, `onUp`, `onWheel`, `onKey`, `draw(ctx)` and `hint()`.
- The viewer keeps the view transform, base drawing and hit-testing, and delegates input to the active mode.
- The v1 interaction code moves into `NormalMode`. This keeps `viewer.js` from growing into one large switch.

**App:**

- `app.edit(label, fn)` stays the single entry point for model changes.
- Bounds edits go through `app.editBounds(label, subjectId, fn)`. It records the changed frame range, truncates affected results and stores the removed data in the undo item.

### 11.2 Engine

| Module | Responsibility |
|---|---|
| `engine.py` | CoTracker streaming (unchanged core) + bounds starting guess in `run_window` (per-track, from the CPU copy of last coords; no extra GPU sync) |
| `frames.py` | `FrameReader` with a `convert` hook (model RGB or full-res gray), exact-frame reads, `FrameServer` (a warm sequential decoder per video + a small LRU of frames) |
| `templates.py` | `TemplateJob`: sequential full-res gray matching per §7.2, emits the same result messages |
| `runs.py` | `RunJob`: starts the CoTracker job on the GPU worker and the template job on its own thread, each with its own decoder; merges status (running while any runs; frame = the slowest; done/halted/error) and forwards results; preview comes from CoTracker (or templates when alone) |

**Decoding twice** when both kinds run costs CPU, but keeps the CoTracker window loop untouched. We revisit it only if profiling says so.

### 11.3 Server and protocol

- `GET /api/videos/{id}/crop?f=&x=&y=&w=&h=&scale=` returns a PNG of an original-resolution region, via `FrameServer`. Regions may extend off-frame (padded). It's used by the template editor and the loupe.
- WebSocket `start` message v2:

```jsonc
{ "type": "start", "runId": 5, "videoId": "…", "startFrame": 120,
  "segments": [
    { "key": "2@0@812.50,440.00", "q": 0, "x": 812.5, "y": 440, "end": null,
      "kind": "point", "subjectId": 1 },
    { "key": "3@0@960.00,540.00#4.1", "q": 0, "x": 960, "y": 540, "end": null,
      "kind": "template", "subjectId": 1, "threshold": 0.7, "looks": [ /* as in the project */ ] }
  ],
  "bounds": { "1": { "f0": 0, "data": "<base64 Float32 cx,cy,w,h per frame>" } } }
```

Result and preview messages are unchanged.

### 11.4 Performance notes

- **Per-frame model queries stay O(1)** (bounds, layers) or O(log n) (offset keys, segments). `subjectState` runs for every timeline column, so the new terms must stay cheap.
- **Undo memory** is bounded by copy-on-write chunks. Result patches in undo items only hold the truncated chunks.
- **Template matching** is local-first. When a template is lost it costs about 10 ms per frame without bounds, less with them.

## 12. Export v2

- **JSON `cotrack.tracks` v2.** `x`/`y` still mean the final position, so v1 readers keep working. Each subject track also gets:
  - `rawX/rawY` (trackers' mean), `offsetX/offsetY`, `finetuneX/finetuneY`.
  - `bounds` as `[cx, cy, w, h]` or null.
  - Counts including `drifted` and `notFound`.
- **Trackers** export `kind`, per-frame samples, `score` and `look` for templates, and a `drifted` flag. Template looks export geometry and hotspot; masks are optional.
- **CSV** gains columns: `raw_x, raw_y, offset_x, offset_y, finetune_x, finetune_y, bounds_cx, bounds_cy, bounds_w, bounds_h, drifted, not_found`.
- **Resolve/Fusion and Premiere/AE exporters** (queued since v1) build on this.

## 13. Phases

Each phase ends with Node unit tests for the model, a Playwright end-to-end run (0 console errors), and a regression pass of the v1 flows.

| Phase | Scope | Acceptance |
|---|---|---|
| **A. Foundations** | v2 model (tracker kinds, chunk stores, copy-on-write undo, result patches), new gestures (drag-select, Ctrl / Ctrl+Alt box end), offset (keys, drag, ghost + dashed line, sidebar, timeline), v1→v2 migration | v1 projects load; box end/undo works; offset exports |
| **B. Template trackers** | `frames.py` + crop endpoint, template editor, looks, `TemplateJob` + `RunJob`, WS v2, display/inspector | On a synthetic clip (cursor sprite composited on footage along an erratic known path): median error < 1 px while visible, correct not-found when hidden, ≥ 150 fps local-search throughput |
| **C. Bounds** | Chunked bounds, display, falloff editing, Puppeteer pass, drift flags + End drifted, template search in bounds, CoTracker starting guess, invalidation | Synthesis unit tests on synthetic mouse traces (lag, jiggle→size, inclusiveness); scripted Playwright puppeteer pass; measurable gain from the starting guess on a fast-motion clip |
| **D. Finetune** | Loupe (+ crop prefetch), sessions, layers UI, timeline rows | Scripted session writes the expected deltas; toggle/weight/undo behave; final = stack formula |
| **E. Crop-mode experiment** | Prototype and measure per §8 | Go/no-go with numbers |
| **F. Polish** | README, help overlay, export docs, Resolve/Fusion export | — |

Order rationale:

- A first, because everything else sits on the new model and undo.
- B before C, because template trackers are useful on their own (cursors) and give bounds something to restrict.
- D last among the features, because it refines the final position the others produce.

## 14. Decisions log

- Box-select moves to plain drag; Shift+drag draws template boxes.
- Ctrl / Ctrl+Alt box: ends trackers from the current frame on (earlier data kept), selected subject only.
- Drifted trackers are flagged and excluded, not auto-ended; one button ends them.
- Template masks are painted in a magnified popup. Several looks per tracker share one hotspot.
- Offset is plain pixels with linear keys.
- Finetune: relative nudging by default (Shift finer), `Q` switches to a targeting loupe. Layers behave like keyed animation that holds its value (no end-of-session drop-off).

## 15. Risks

- **Scope:** mitigated by phase gates; each phase is usable on its own.
- **Puppeteer feel is subjective:** tunables are exposed; tune with real footage.
- **Starting guess could hurt when bounds are poor:** per-subject toggle (`boundsGuide`).
- **Crop mode may not pay off:** it's gated behind measurement.
- **Browser pointer and rVFC timing varies by machine:** the synthesis only needs roughly correct timestamps, and lag is tunable.
