# trackertools v2: design

Status: draft for review, 2026-09-27. Companion: [ROADMAP.md](ROADMAP.md).

v2 is a ground-up rebuild in Rust. The v1 "CoTrack editor" (`editor/`, Python + browser) is legacy: it is a source of proven algorithms and known pitfalls, not an architecture to preserve. Naming trap: v1's own `editor/PLAN.md` is titled "v2 — guided tracking plan". This document is the actual v2.

> **Philosophy.** Tracking objects in video is impossible to fully automate, but it can be made a hell of a lot easier and smoother with smart human guidance. This is a tool in service of creating, editing and outputting tracking data from 2D video.

**Scope.**
- **One project = one source video.** Anyone can splice footage beforehand.
- **The deliverable:** frame-accurate tracking data for **multiple subjects**, matched to that source clip's frame grid, in source pixels, ready to link back to the clip in an NLE.

---

## 1. Principles

1. **The world is the only source of truth.** Everything lives in one ECS world:
   - the document (media, captures, views, trackers, targets);
   - the session (playhead, selection, viewports, the active tool, panel layout);
   - the machinery (jobs, caches).

   No subsystem keeps private state that another subsystem needs.
2. **Everything visible is derived.** Panels, overlays and the timeline are functions of the world, re-evaluated every frame (immediate mode). They never own state; they emit *intents*.
3. **Raw human input is sacred.** A capture keeps every pointer sample with its wall-clock timestamp, forever. Smoothing, sizing and views are re-runnable *operators* over that record, so every parameter stays adjustable after the fact. After Effects bakes Motion Sketch into keyframes and discards the raw input; we don't.
4. **Every transformation is an operator.** It has explicit inputs, parameters, an output, and a declared temporal footprint. That uniformity makes data convertible and reusable, lets invalidation be computed rather than hand-written, and puts every intermediate result in reach of the UI.
5. **Time and space are explicit.**
   - Every signal declares its time domain (video frames or wall-clock time).
   - Every position declares its coordinate space (the source frame or a view).
6. **Direction-agnostic by construction.**
   - Operators declare whether they are causal, anti-causal or windowed.
   - Frame sources can serve frames forward or backward.
   - Reverse tracking is a parameter, not a separate feature.
7. **Extend by adding modules, not by editing the core.** A new filter, tracker, view type, panel or tool is one module that registers itself (§11).
8. **Document state is undoable; derived state is recomputed.** Undo never has to know about caches.

---

## 2. Architecture at a glance

```
              ┌──────────────── bevy_ecs World ────────────────┐
 winit input  │  entities + small components + relationships   │   egui panels & overlays
 ───────────► │  resources: Transport, Selection, Registry, …  │ ──────────────────────────►
 (timestamped)│                                                │   (read world, emit intents)
              │   SignalStore (resource): chunked per-frame /  │
              │   per-sample data, copy-on-write, versioned    │
              └──────┬───────────────────────────▲─────────────┘
                     │ demand + dirty ranges      │ result chunks (single writer)
              ┌──────▼──────────┐          ┌──────┴──────────┐
              │ Operator eval   │          │ Jobs & workers  │  decode (ffmpeg-sidecar),
              │ (sync, budgeted)│          │ (threads/procs) │  proxy build, trackers (later)
              └─────────────────┘          └─────────────────┘
```

**Frame loop.** Each app frame runs two schedules around the egui pass. Order is deterministic:

| Set | What runs |
|---|---|
| `Input` | drain raw winit events into `InputEvent` messages, with precise timestamps |
| `Transport` | advance the playhead from the wall clock when playing; record ClockMap segments while capturing |
| `Tools` | the active tool reads input; gestures open / update / commit transactions |
| `Intents` | apply intents emitted by panels on the previous egui pass |
| `Invalidate` | propagate dirty frame ranges through the operator graph |
| `Evaluate` | recompute demanded dirty ranges of cheap operators within a time budget (≈4 ms) |
| `Jobs` | start / cancel / retarget jobs; merge finished result chunks into the SignalStore |
| `Media` | request decodes for visible frames; upload textures |
| `Prepare` | derive render data (timeline summaries, overlay geometry) |
| *(egui pass)* | panels draw from the world and emit intents |
| `PostUi` | apply latency-critical intents (drags) in the same frame |

---

## 3. Time

- **FrameIndex** (`i64`): a position on the media's constant frame grid. The grid rule comes from v1: frame *i* is the decoded frame whose timestamp rounds to `i / fps` after subtracting the first frame's timestamp. Variable-frame-rate gaps repeat the previous frame.
- **Rational rates.** fps is stored as a rational (e.g. 60000/1001). Conversions use `RationalTime`-style arithmetic, as in OpenTimelineIO, so there is no floating-point drift.
- **Wall time** (`f64` seconds since the session epoch) stamps raw input.
- **Transport** (a resource): `playing`, `rate` (fraction of real time), `direction`, loop range, playhead (FrameIndex plus a sub-frame phase).
- **ClockMap:** the recorded mapping from wall time to video frames during a capture. It is a list of segments `{wall_start, wall_end, frame_at_start: f64, rate}`, where `rate` is frames per wall-second.
  - playing at 50% → `rate = 0.5·fps`
  - paused → `rate = 0`
  - a step or a scrub → a discontinuity (a new segment)

  **Any transport behaviour during a capture is therefore representable**: slowed playback, pausing, stepping, scrubbing back and forth. This one structure is what makes hold-to-simulate (§8.3) fall out naturally.

---

## 4. Spaces and views

- **Coordinates.** Continuous pixels: (0,0) is the top-left *corner* of the top-left pixel, and pixel centres sit at +0.5. All resampling (display, crops, model inputs) must use this same convention; v1 mixed area-resize with align-corners maps and got a ~1.4 px bias at the frame edges.
- **Space.** Every positional signal carries `Space::Source(media)` or `Space::View(entity)`.
- **View** (an entity): a per-frame transform from its parent space into its own space. The source frame is the root view of each media.
  - Views form a tree through a `ViewOf` relationship (child → parent).
  - The source→view transform is the composition along the chain, cached as a derived signal.
  - The transform starts as a similarity (uniform scale + translation); rotation and homography can be added later without changing the model.
- **Viewport** (a session entity, one per viewport panel): which view it shows, plus zoom/pan within that view, overlay toggles, and follow options. Switching a viewport to a derived view is a component change, so it is undoable only if we choose to make it so; by default it is session state.
- **Rendition spaces.** A rendition (the original, or a proxy at scale *k*; see §13) is also a space: `x_rendition = x_source · k` exactly, with the same corner-origin convention. Results computed on a proxy therefore lift to source pixels without error. Only the *measurement* is coarser, never the mapping.
- **Lift / project.** Moving a position between spaces applies the (inverse) transform chain at that sample's frame. Tools that operate inside a view store what the user did in that view's space and lift it only when an operator needs it elsewhere.

---

## 5. Data: signals, streams, keys

Big data lives in the `SignalStore` resource, outside the ECS archetypes. Components hold small handles (`SignalId`). This keeps component moves cheap, gives undo cheap copy-on-write snapshots, and lets workers read slices without copying.

- **Signal:** a per-frame channel set with a fixed layout (e.g. `Pos2 = [x, y]`, `Extent2 = [hw, hh]`, `Similarity = [s, tx, ty]`, `Conf = [c]`, flags). It is stored in **256-frame chunks** of `f32`. Each chunk carries:
  - a validity bitmap (which frames are computed or present);
  - a `version`;
  - a `provenance` id (the operator run that wrote it).

  Chunks are `Arc`-shared and copy-on-write. A 70k-frame channel is about 272 chunks.
- **Stream:** an append-only, irregularly sampled record, chunked the same way. Example: pointer samples `{t_wall: f64, x: f32, y: f32, buttons, modifiers}` in the space they were drawn in.
- **Keys:** sparse keyframes (a small component) with per-key interpolation (hold, linear, bezier with handles). Keys are *not* a signal; the `Keys` operator (§7) turns them into one.
- **Coverage and staleness.** Each output signal records per frame: *absent*, *valid* or *stale*. Stale means an input changed and the value is kept for display (drawn with stale styling) until recomputed: stale-while-revalidate. Expensive results (trackers) are never deleted just because something upstream moved.
- **Summaries.** For the timeline, each signal maintains a multi-resolution pyramid of per-block summaries (coverage, min/max, worst status per 2^k frames). This fixes v1's aliasing, where one sample per pixel column on a 69k-frame clip hid short failures.

---

## 6. Operators

An operator is an entity with:
- an `Operator` marker;
- a kind-specific **params** component (reflectable, so it is editable in the inspector for free);
- an **`Inputs`** component (named slots → source entity + signal);
- **outputs** (owned signals).

Hooks on `Inputs` maintain a reverse `Dependents` index, which is used for invalidation.

Each kind registers a vtable (`OperatorKind`):

```rust
trait OperatorKind: Send + Sync + 'static {
    fn footprint(&self, slot: Slot, dirty: FrameRange, extent: FrameRange) -> FrameRange;
    fn evaluate(&self, ctx: &mut EvalCtx, range: FrameRange) -> Result<()>; // sync kinds
    fn cost(&self) -> Cost; // Sync { per_frame_ns } | Job { .. }
}
```

**Footprint classes** decide what an upstream edit on frames `[a, b]` invalidates downstream:

| Class | Invalidates | Examples |
|---|---|---|
| Pointwise | `[a, b]` | offset, space lift, influence blend |
| Window(−p, +q) | `[a−q, b+p]` | Gaussian / zero-phase smoothing, union-over-window |
| Causal | `[a, end]` | forward trackers, integrators, causal springs |
| Anti-causal | `[start, b]` | reverse trackers, backward passes |
| Global | the whole extent | key fitting, max-over-pass, ClockMap-dependent capture stages |

**Evaluation is demand-driven.**
- Consumers post demand ranges: viewports want the playhead ±N frames, the timeline wants its visible range at summary resolution, export wants everything.
- `Evaluate` recomputes dirty ∩ demanded ranges in topological order, within budget.
- Anything left over carries into the next frame and shows as a partial state, never as a hitch.
- `Job`-cost kinds are not evaluated inline; they get a Job (§9).

### 6.1 Typed ports and frame sources

Operator inputs and outputs are **typed ports**. This is the whole "node system": no generic node library at the core, just typed edges between entities.

| Port type | Carries |
|---|---|
| `Frames` | pixels from a **FrameSource** (below) |
| `Track` | position + confidence, in a declared space |
| `Extent` / `Box` | region size / a Track + Extent |
| `Transform` | a view's per-frame transform |
| `Keys` | sparse keyframes |
| `Stream` | raw irregular samples (pointer input) |
| `Scalar` / `Flags` | anything else per-frame |

- **A FrameSource is a first-class entity, the "input node" for anything that looks at pixels.** It has four parts:
  - **rendition:** the original, a proxy at scale ½, ¼ …, or **Auto**;
  - **view:** crop through any view in the tree (optional);
  - **output size:** what the consumer wants, e.g. CoTracker's 512×384 or a template matcher's native size;
  - **direction:** forward or reverse.
- **Auto picks the rendition like a texture mip level.** It chooses the smallest rendition that still gives at least one real pixel per output pixel for that view's crop. The consequences:
  - a full-frame tracker whose model runs at 512×384 anyway reads a ¼ proxy and loses nothing;
  - a tracker working in a tight, zoomed view automatically reads the original;
  - a user can pin a rendition per tracker ("track at ½ for speed") when they want to trade accuracy for speed.
- **Implicit lifts.** Connecting a `Track` port in one space to a consumer in another inserts a `Lift` adapter automatically, so composition across views and renditions just works.
- **Coarse-to-fine is plain composition.** Tracker A runs on the ¼ proxy. Its Track feeds tracker B's *guide* input, and B refines at full resolution in a small window. That's classic pyramid tracking, built from two ordinary operators and needing no special feature.
- **A graph view** (egui-snarl) can visualize and edit these connections later. Day-to-day, connections are made by tools and commands ("add tracker to this view", "track at ½"), not by wiring nodes.

**Result caching** (planned for after M4): results keyed by `(kind, params hash, input chunk versions)` in a content-addressed store. Undo and redo then re-link earlier results instead of recomputing, and A/B-ing parameters becomes free.

---

## 7. The concept catalog (how everything fits)

The whole editor is built from a handful of entity archetypes connected by operators.

| Concept | Is | Produces |
|---|---|---|
| **Media** | a video file + frame index (+ proxy) | frames, via a frame source (forward/backward) |
| **View** | a per-frame transform; a node in the view tree | a space; what a viewport shows |
| **Capture** (motion sketch) | raw pointer Stream + ClockMap + the view it was drawn in | a pipeline of operators (§8) |
| **Track** *(a role, not a kind)* | any entity exposing `TrackOutput { pos, conf, space }` | a position signal with confidence |
| **Box** *(a role)* | a Track plus an `Extent` signal | a region over time |
| **Target** (subject) | a `Combine` operator over **Contribution** edges (entities: source track, weight, influence, mode) | the final coordinate |
| **Modifier** | any Signal→Signal operator: smooth, offset (keys), lag, wiggle, influence blend | a new signal version, stackable |
| **Job** | an operator output being materialized over a range, in a direction | chunks, progress, cancellation |

Consequences:
- **"A human animation is a tracker"** falls out for free. A `Keys` operator (keyframes → signal) exposes `TrackOutput`, just as a smoothed capture centre does, and later CoTracker, the template matcher, or a Kalman-fused combination will.
  - Anything that accepts a Track accepts all of them: a Target's contribution, a view's framing, a drift check, an export.
- **Conversions are operators too:**

  | Operator | Converts |
  |---|---|
  | `Resample` | stream → frames, via the ClockMap |
  | `Bake` | keys → samples |
  | `Fit` | samples → keys, with After Effects Smoother-style tolerance |
  | `Lift` / `Project` | between spaces |
  | `Frame` | box → view |

  Any data can be turned into any other representation in one step, and the source stays intact.
- **Views nest by construction.** A capture drawn inside view V produces a box in V's space. Its `Frame` operator produces view W with `ViewOf(W, V)`. Trackers can attach to any view in the tree. Their results are lifted to source space through the chain, so a tracker working in a tight, stabilized view still produces source-pixel output.

---

## 8. Flagship: motion sketch

### 8.1 Capture: recording like auto-key

*(Reworked in M3 after hands-on use: a press no longer starts anything. The transport stays the user's, and the tool records against whatever is on screen, like auto-keying in an animation package.)*

- **Tool:** *Sketch*, default key `D` (Blender's draw/annotate key; `S` belongs to Blender's scale, see §18).
- **Recording:** press and hold on a viewport. Every pointer report is recorded (1 kHz raw input, timestamped; not one sample per UI frame), against whatever frame is on screen:
  - **paused:** the hold edits that instant (hold-to-simulate, §8.3);
  - **playing:** tap `Space` while holding and it records across frames, at the playback rate (the capture speed, set with `[` / `]`; slow motion is just a rate). Tap again to pause and keep shaping that frame;
  - steps, jumps and scrubs while holding are recorded too.

  The ClockMap records every transport change, so all of it maps back to video frames.
- **New sketch:** the `＋ New sketch` button (next to the Sketch tool), `Shift`+hold, a click on empty video, or `Alt+A` (deselect) all make the next stroke start a new sketch instead of editing the selected one.
- **Strokes and sketches:** each press → release is a *stroke* (a Capture entity plus a `Stroke { falloff, influence, size }` component). It goes onto the **selected sketch**, so editing a rough path means: select it (a click on its box, its timeline lane, or the outliner), go to a frame, press and hold or drag. With nothing selected, or with `Shift` held at the press, the stroke starts a new sketch. `Alt+A` deselects.
- **Clicks select, holds record:** a press shorter than 0.18 s that moves less than 4 screen points is a click, in any tool. It selects the sketch whose box is under it (the smallest where boxes overlap), or clears the selection on empty video, and never records. Only a hold or a drag edits, so a stray click can't change a path.
- **Move only:** `Ctrl` at the press makes the stroke keep the box size that was there (`size = 0`). Without it, a hold also sets the size from its jiggle (§8.3), so a quiet hold makes the box tight. Both stay editable per stroke.
- **Layering (proportional editing in time):** a sketch's strokes are laid over each other in order:
  - frames a stroke visited take its value (blended by `influence`, like an NLA strip);
  - frames within `falloff` of a visited run keep their own motion but move by the run's edge offset, with Blender's smooth falloff curve. Where several edits reach one frame, the weights are normalised, so the frames between two edits with the same offset move by exactly that offset (no overshoot);
  - where there is no path yet, a gap of at most twice the falloff between the stroke and another value is bridged linearly, so a path can be blocked out with holds on key frames;
  - the mouse wheel sets the falloff while holding (shown in the HUD and on the timeline). Every stroke stays re-tunable, and removing one restores what was under it.
- **Live feedback:**
  - the raw hand trail;
  - the sketch with the stroke laid over it (path and region), computed by the same pipeline over the samples so far;
  - the other sketches, faint;
  - on the timeline, the frames the stroke visits and the frames its falloff moves.

  The box outline follows After Effects' "Show Wireframe".
- **Takes and levels** carry over from v1 (to do):
  - "take again" at the same level averages robustly (weighted median centres, log-space sizes);
  - disagreement widens the box a little and marks those frames *uncertain*.
- **Release:** the stroke is committed as one undo step. Frames *played* in the last `lag` before the release get no result: the hand never reached them, and a catch-up (Krita's "finish line") would invent positions for them. Because the smoothing is zero-phase with padded ends, no lag offset remains to catch up elsewhere. *(Changed from a planned catch-up after the first measurements, M3.)*

### 8.2 The sketch pipeline (all operators, all re-tunable)

Hand noise and lag live in *real* time, so the hand-related stages run in the wall-time domain. Everything after `Resample` is per video frame.

```
PointerStream (wall) ─► Lag ─► Smooth(center) ─► Resample(ClockMap) ─► Union(±window) ─► center, extent : Signals
                          └─► Jiggle(extent) ─┘                                         └─► Frame ─► View
```

| Stage | Parameters (defaults) | Source |
|---|---|---|
| **Lag** | `lag` 0.25 s wall; later an optional auto-estimate by cross-correlating against a tracker | v1 |
| **Smooth (centre)** | **Steadiness** (One Euro `min_cutoff`, Hz) and **Responsiveness** (`beta`), tuned in that order; **Dead zone** (px, in the drawn-in view's space; ignores tremor); offline refinement is zero-phase (forward–backward), so it is lag-free and reverse-symmetric | One Euro filter (Casiez's tuning procedure), Blender lazy mouse, SciPy `filtfilt` |
| **Jiggle → extent** | RMS spread of the hand around a *slow, non-adaptive* reference (the steadiness cutoff alone), window σ 0.25 s; **Gain** 1.0; **Pad** 12 px; **Min half-size** 16 px. *(M3: measuring against the responsive point path under-read a jiggle, by a different amount while paused than while playing, so an edit made while paused came out 2–3× smaller. Against the slow reference the same jiggle reads the same in both.)* Still to do: a critically damped spring for grow/shrink | v1 synthesis, Screen Studio-style springs |
| **Resample** | how multiple samples on one frame (pauses, re-scrubs) combine: *last pass wins* (default) or *average* | ClockMap (§3) |
| **Union** | include motion over [f − 0.1 s, f + 0.15 s], applied to the whole sketch *after* its strokes are layered, so a frame edited while paused picks up the path's motion from its neighbours like a recorded frame | v1 |

Every stage's output signal is inspectable: raw, lag-shifted, smoothed, extent. The Smoothing panel shows raw vs smoothed trails live while you drag a slider. Presets (*Tight / Default / Loose*) sit over the numbers, following Premiere's Auto Reframe presets.

### 8.3 Hold-to-simulate while paused

1. With the video paused and the Sketch tool active, **press and hold** on the subject.
2. The ClockMap records a held segment on the current frame.
3. Samples keep streaming in wall time. Jiggle measures spread over its wall-time window, so the box at this frame grows while you jiggle and settles tight when you hold still: a "simulation step" in the Houdini live-tick sense. The same jiggle gives the same size as it would while playing, and on an existing sketch the frame also takes the motion union from its neighbours, so a paused edit reads like a recorded frame.
4. The frame takes the hand's state at the end of the hold, with no lag shift, since the hand has settled on what is shown. Before `lag` has passed, it uses the time the hand needed to get there, `start + lag`.
5. It combines with stepping: keep holding, press `→` to step a frame, and keep shaping, sculpting a box frame by frame through a difficult passage. On an existing sketch, each held frame's change spreads to its neighbours through the falloff (§8.1).

---

## 9. Flagship: smoothing as non-destructive modifiers

Any signal (a capture centre, a tracker output, a target, a view path) can carry a **modifier stack**, as in Blender's F-curve modifiers:

- **Smooth** (Gaussian / zero-phase One Euro / RTS)
- **Offset** (keys)
- **Lag** / **Lead**
- **Wiggle** (the inverse: add controlled noise, from the After Effects Wiggler)
- **Clamp**, **Influence**

Each modifier is an operator entity in an ordered chain, with an enable toggle and an animatable **influence** (as on Blender's NLA strips). Removing a modifier restores the input exactly. **Fit to keys** (After Effects Smoother tolerance) converts a smoothed signal into editable keys when you want hand control, and **Bake** goes the other way.

---

## 10. Flagship: derived views

*(As built in M4.)* A **view** is a `frame` operator on a sketch: per frame, a crop of the source `[cx, cy, crop_w, crop_h, canvas_w, canvas_h]` in source pixels. Its *canvas*, the view's own pixel grid, is as big as its widest crop ("display size = max size"): 1 view pixel = 1 source pixel at the widest framing, and the view magnifies as the crop shrinks. Views keep the video's aspect.

- **Every view maps straight to the source** (`SpaceMap`: `source = a · p + b`), so nesting doesn't need a transform chain. The chain is provenance: the breadcrumb, the parent's influence and zoom limits.
- **Strokes drawn in a view are stored through it.** The hand pipeline (lag, smoothing, jiggle) runs in the view's pixels, where the hand actually moved. Each stroke records the view's mapping for every frame it touched (`Through`), and its per-frame results reach the source through that record.
  - Consequence: re-tuning a parent view never moves a child sketch that already tracks something. *(Changed from "children re-derive when parents are re-tuned": a face track must not drift because the body's camera damping changed.)*
- **The motion union of a sketch drawn in a view is measured in that view** (its *home*, input `space`). In a stabilized view the subject barely moves, so nested sketches stay tight.
- **Entering views:** `Tab` enters the selected sketch's view, creating it on first use as one undo step, and clears the selection. So a hold inside a view starts a new sketch nested there. To edit the sketch that defines the view from inside it, click its box first.
  - `Shift+Tab` backs out to the parent and selects the sketch just left, so `Tab` goes straight back in.
  - A breadcrumb (`Source ▸ Sketch 1 ▸ …`, on its own layer so its clicks never reach the video) jumps to any level.
  - Each view keeps its own zoom and pan.
  - Outside the frames a sketch covers, its view holds the nearest framing, labelled in the breadcrumb.
  - A change of framing at the frame being looked at (an edit to the view's own sketch, a re-tune) eases in over 0.25 s instead of snapping.
- **The region always fits:** `fit` wins over the parent's influence and the zoom limits.
- **Measured** (tests/view.rs and the in-app demo):
  - a root view keeps the sprite in its central 30% on 100% of frames;
  - three levels deep, the deepest view does too;
  - a sketch drawn inside a view is about 3× more accurate: 0.5–0.7 px vs 1.5–2 px median in source pixels.

### 10.1 Framing a view from a box

The `Frame` operator (a virtual camera) turns a Box into a View transform. Its parameters are Cinemachine's framing controls:

| Parameter | Meaning | Source |
|---|---|---|
| **Display size** | the view's fixed output size = the **max extent during the pass**, using a *slowly decaying* running max so one jiggle spike doesn't lock the zoom out for the whole pass. *(Built, in log space: the view zooms out ahead of the region growing, up to ×2 per `lead` 0.25 s, and back in slowly after it shrinks, ×½ per `hold` 1 s. Then a max filter over ±`zoom_damping` 0.5 s and a Gaussian that stays inside that window, so the crop is smooth and never smaller than the region needs; a jittery region size makes no zoom jitter. The canvas is the widest crop.)* | user spec + Cinemachine group framing |
| **Fit** | how much of the display the box fills (default 0.6; the region always fits) | Cinemachine framing size |
| **Dead zone / soft zone** | the box can move within the dead zone without the camera reacting; beyond it, the camera re-centres at the damping rate | Cinemachine position composer |
| **Damping (pan, zoom)** | separate; *zero-phase* mode (lag-free, offline) or *causal* mode (camera-like) | Cinemachine, SciPy `filtfilt` |
| **Zoom clamp** | minimum and maximum scale relative to the parent | Cinemachine min/max |
| **Influence (location, scale)** | 0 = static parent framing, 1 = fully follow; blends partial stabilization without re-tracking | Blender 2D stabilization |
| **Reference frame** (optional) | anchor the view to how the box sat at frame N | Nuke stabilize |

### 10.2 Using views

- **Switch a viewport to a view:** from the outliner, a breadcrumb bar (`Source ▸ Sketch 1 ▸ Sketch 1.2`), or a key to step up and down the chain.
- **Adjustable inside the view:**
  - edits made while looking through a view (moving keys, re-sketching a segment, nudging an offset) happen in that view's space;
  - the stored data stays in the space it belongs to;
  - the display re-derives live.
- **Nested sketching:** sketching inside view V yields a capture in V's space, then a Box, then view W as a child of V.
- **Rendering:** the viewport draws the media frame through the composed source→view transform in a shader.
  - It samples the **original** full-resolution frame when the effective zoom exceeds the proxy's resolution, and the proxy otherwise.
  - Out-of-frame areas render as a neutral pattern, not black, so off-frame regions are obvious.
- **Blending:** a nested view can blend against its parent by influence instead of hard-switching (from Blender's NLA).

---

## 11. Extensibility: modules

A feature is a module:

```rust
pub trait Module { fn build(&self, app: &mut AppBuilder); }

// inside a module's build():
app.component::<SketchParams>(Meta::document())   // persisted + undoable + inspectable
   .component::<LiveCursor>(Meta::session())      // in the world, not undoable
   .operator::<OneEuroSmooth>()
   .systems(Set::Tools, sketch_tool_system)
   .tool::<SketchTool>(Key::S)
   .overlay::<SketchTrailOverlay>()                // drawn in any viewport showing a capture
   .inspector::<SketchParams>(sketch_params_ui)    // optional custom UI; reflection fallback otherwise
   .timeline_lane::<Capture>(capture_lane)
   .intent::<RetuneCapture>(apply_retune);
```

- **The reflection-driven inspector** means a new component is editable, and its edits undoable, with zero UI code.
- **Overlays and timeline lanes are registered per component**, so panels never import feature code.
- **Core modules (first target):** `time`, `media`, `views`, `sketch`, `smoothing`, `timeline`, `inspector`, `persist`.
- **Later modules:** `trackers/template`, `trackers/learned` (Python worker), `targets`, `export`.

---

## 12. Undo, redo, persistence

- **Component classes** are declared at registration:
  - **Document:** persisted and undoable.
  - **Session:** in the world and persisted in session settings, but not undoable (viewport zoom, panel layout, hover).
  - **Derived:** never persisted or undone; recomputed.
- **Selection** is session state, but every transaction snapshots it, so undo restores the context you were working in.
- **Transactions.** All document mutations go through `world.edit("Label", |tx| …)`. The transaction records, via bevy_reflect:
  - component values before and after;
  - spawned and despawned entities (with full reflected bundles);
  - SignalStore chunk references (copy-on-write, so snapshots are cheap).
- **Gestures** (drags, captures) hold one open transaction for their lifetime, so a whole gesture is one undo step. In debug builds, an audit compares change detection against recorded transactions and flags untracked document mutations.
- **Project file:** a single SQLite database (`*.ttproj`).
  - Entities carry `StableId(Uuid)`; entity references are mapped through it.
  - Components are stored as versioned RON via reflection.
  - Signals and streams are stored as content-hashed, zstd-compressed chunk blobs.
  - Saves are incremental (only dirty chunks), transactional and crash-safe.
  - Schema migrations are versioned from day one.
- **Media paths** are stored both absolute and relative to the project, fixing v1's orphaned-project problem.

---

## 13. Media pipeline

Adopted from Rerun's proven design.

1. **Index.** `re_mp4` parses the container into a full sample table: pts, dts, size, offset and keyframe flag for every sample. The frame grid, GOP starts and B-frame order are derived from it. Other containers (MKV and so on) are remuxed to MP4 on import with `ffmpeg -c copy`; no re-encode.
2. **Decode.** `ffmpeg-sidecar` runs a bundled or system `ffmpeg.exe` subprocess. We feed it Annex-B packets from our own index (as Rerun does) and read NV12 frames from its stdout, with `-hwaccel` where available. Frame N comes from decoding forward from the nearest preceding keyframe.
   - This involves no FFmpeg linking, no bindgen or vcpkg, and a clean license.
   - AV1 would come later via `rav1d`.
3. **Decoders.**
   - a *playback* decoder (sequential, read-ahead);
   - a *seek* decoder (GOP-aware, latest-request-wins coalescing, from v1's player);
   - later, *job* decoders for trackers, including reverse: decode a keyframe interval forward, emit it reversed.
4. **Frame cache.** NV12 frames in RAM, with an LRU budget (default 2 GB) and a read-ahead / read-behind window around the playhead. Scrubbing shows the nearest cached frame immediately, then the exact one.
5. **Display.** NV12 planes are uploaded to wgpu textures and converted to RGB in a fragment shader, inside an `egui_wgpu::CallbackTrait` paint callback. View transforms are applied in the same shader. Zero-copy hardware decode (Media Foundation + D3D11) is a later optimization, only if profiling demands it.
6. **Renditions (a mip pyramid of the video).** Background NVENC transcodes, built automatically for long-GOP sources and shown in the Jobs panel:
   - a **scrub proxy**: 720p-class, keyframe every 12 frames, no B-frames; makes backward stepping and scrubbing instant;
   - **tracking renditions** at ½ and ¼ scale, created on demand when a FrameSource needs them.

   All renditions share the source's frame grid, and each is an exactly scaled space (§4). The original stays authoritative. FrameSources choose a level per use (§6.1), and the display samples the original when zoomed past the proxy's resolution.
7. **Audio** is deferred. When added, it will use rodio/cpal with an audio-sample clock as master.

---

## 14. UI

- **Shell:** eframe (wgpu backend) + `egui_tiles` docking. The layout is session state in the world.
- **Panels** (each a function of the world that emits intents):
  - **Viewport** (N instances): shows a View; overlays come from registered overlay providers.
  - **Timeline:** a ruler, transport, lanes per entity from registered lane providers, summaries (§5), and key editing.
  - **Inspector:** reflection-driven, plus custom widgets.
  - **Outliner:** media → views tree → captures / trackers / targets.
  - **Jobs/status.**
  - Later: **Curve editor**, **Operator graph** (egui-snarl).
- **Tools** (Select, Sketch, Adjust, Pan/Zoom) are state machines in the `Tools` set, fed by a `PointerFrame` resource (every timestamped sample since the last frame, in source pixels) and `KeysHeld`, so they run headless in tests. An in-progress gesture is world state (the Sketch tool's `LiveCapture`), so overlays draw it; on commit it becomes document entities in one transaction.
- **Keymap** is data (a resource), rebindable, with a help overlay generated from it. Defaults are **Blender-like**:
  - `Space` play (also while holding the button: recording across frames);
  - `D` Sketch tool; click selects; `Shift`+hold starts a new sketch; `Ctrl`+hold moves only; the wheel sets a stroke's falloff while holding; `Esc` cancels the stroke or leaves the tool;
  - `Alt+A` deselects;
  - `[` / `]` playback rate (= capture speed);
  - `←/→` step, `Shift+←/→` jump to start/end;
  - `G` / `S` grab / scale selected;
  - `X` delete;
  - `Ctrl+Z` / `Ctrl+Shift+Z` undo / redo;
  - `A` select all;
  - `N` toggles the sidebar;
  - `Home` frames all.

  They'll be tuned as we go.

---

## 15. Performance budgets

| Scenario | Target |
|---|---|
| UI frame (1080p viewport + timeline, 70k-frame clip) | ≤ 8 ms CPU |
| Playback 1080p60 H.264 at 1× | 60 fps with no dropped frames (4K60 is a stretch goal) |
| Step forward (cached) / step back with proxy / step back on the original with 250-frame keyframe interval | ≤ 1 frame / ≤ 50 ms / ≤ 250 ms worst case |
| Pointer capture | every OS event, timestamp resolution ≤ 1 ms |
| Re-tune a 60 s capture | ≤ 5 ms to re-derive; the view updates the same frame |
| Open the 69,470-frame P5 file (index) | ≤ 2 s |

---

## 16. Workspace layout

```
Cargo.toml                 # workspace; pinned versions
crates/
  tt_core/    # headless: module system, time, spaces, SignalStore, operators, invalidation,
              #           transactions/undo, persistence, and domain data + systems (views, sketch, smoothing)
  tt_media/   # re_mp4 index, ffmpeg-sidecar decoders, frame cache, proxy builder
  tt_app/     # eframe binary: shell, panels, overlays, tools, input tap, wgpu video rendering
tools/        # test-fixture generators (ffmpeg lavfi: frame-counter clips, long-GOP, VFR, moving sprites)
docs/v2/
```

- `tt_core` has no UI or GPU dependencies, so everything that matters is testable headless with `cargo test`.
- v1 (`editor/`, `cotracker/`) stays untouched until v2 supersedes it. Its CUDA CoTracker engine returns later as the learned-tracker worker.

**Pinned stack** (crates.io, checked 2026-09-27):

| Crate | Version | Notes |
|---|---|---|
| `bevy_ecs`, `bevy_reflect` | 0.19.1 | 0.20 is at release-candidate stage; migrate when final, as Bevy renames APIs per release |
| `eframe`, `egui`, `egui-wgpu` | 0.36.2 | |
| `egui_tiles` | 0.17.1 | |
| `wgpu` | 30 | |
| `re_mp4` | 0.5.1 | |
| `ffmpeg-sidecar` | 2.5.2 | |
| `rfd` | 0.17 | |
| `rusqlite` | 0.40 | |
| `egui_kittest` | 0.36 | |
| `tracing` | | |

Toolchain: Rust 1.97.1 MSVC (MSRV 1.95).

---

## 17. Prior-art ledger

| Concept | Taken from |
|---|---|
| Record raw motion on a separate clock; the capture-speed idea | After Effects Motion Sketch |
| Tolerance-based fit to keys; keys vs baked samples | After Effects Smoother, Nuke/Blender curve editors |
| Controlled noise as the inverse of smoothing | After Effects Wiggler |
| Steadiness / responsiveness tuning order | One Euro filter (Casiez) |
| Dead zone + follow factor; catch-up at release | Blender lazy mouse, Krita finish line, Photoshop catch-up |
| Critically damped size response | Screen Studio-style cursor-follow springs |
| Dead / soft zone, damping, framing size, zoom clamps | Unity Cinemachine |
| A view derived from tracks, with per-channel influence | Blender 2D Stabilization |
| Reference frame for stabilization | Nuke Stabilize |
| Non-destructive modifier stacks; influence blending | Blender F-curve modifiers and NLA |
| Trackers feeding separate consumers by live link | Nuke Tracker → CornerPin / Stabilize |
| Views as data; ECS-shaped time-indexed store; egui_tiles | Rerun |
| Demux-own-index + ffmpeg subprocess decode; short-keyframe proxy | Rerun `re_video` |
| Explicit absent / occluded gap semantics | CVAT |
| Rational time | OpenTimelineIO |
| Sampling rate during capture; pushed position; takes and levels; motion consistency | v1 |

---

## 18. Open questions

Resolved 2026-09-27:
- **Project scope:** one source video per project; multiple subjects.
- **Proxies:** automatic, plus tracking renditions with Auto level selection (§6.1).
- **Keymap:** Blender-like defaults, rebindable.

Still open:
1. **Sketch key vs Blender's `S` (scale).** Candidates: `D` ("draw"), or a pen-style tool toggle. Decide when M3 starts.
2. **Timestamp source:** do winit's pointer events on Windows carry OS timestamps precise enough to use, or do we stamp on receipt? Spike M1-S3.
