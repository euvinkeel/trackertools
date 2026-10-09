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
- **Transport** (a resource): `playing`, `rate` (fraction of real time), `reverse`, loop range, playhead (FrameIndex plus a sub-frame phase).
- **Shuttle** (J / K / L, as in DaVinci Resolve): L plays forward and J backward at 1×; each further press in the same direction doubles the speed (2×, 4×, up to 8×), and the other key turns around at 1×. K plays or pauses. When playback stops (K, Space, a step, a seek, the end), the rate set before the shuttle (the capture speed, Q / E) and forward come back. *(Added on request: "DaVinci like playback controls".)*
  - Playing backward, the decode service fills the frames behind the playhead a keyframe group at a time: one ffmpeg start per group, read through to the frames needed (`tt_media::player`).
  - A stroke recorded while playing backward is played through like one played forward: every frame passed takes the moment its centre was on screen (the ClockMap is direction-agnostic). Anticipatory speed reads ahead in the direction of play.
- **In and out points** (`tt_core::marks`, as in an editor): the part of the video an export renders. `I` marks the in point at the playhead, `O` the out point, `Alt+X` clears both, `Shift+I` / `Shift+O` go to them. Both frames are included; an end not marked is the video's own; marking an in point after the out point (or an out before the in) clears the other one. They are document state, saved with the project (in `ProjectMeta`, through `Tx::modify_resource`), and marking is an undo step like any edit. *(Added on request: "define a in/out range for export, visualized thru viewport and scrubbable".)*
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

### 5.1 Lifetimes (spans)

*(Built after hands-on use: lifetimes on the timeline were "sorely missing".)* Any timeline object (a tracker, a sketch, a view) can be told when it begins and ends. That is a **`Span { first, last }`** component on the entity (`tt_core::span`), each edge optional: an untrimmed edge follows the data, so a sketch that gets a stroke past its end still grows there.

- **Non-destructive.** The span never touches the entity's signal. Readers see the output through it (`span::output`, and `EvalCtx::input` for operators: a cheap copy that shares every chunk but the two at the edges), so nothing outside the span reaches a view, an overlay, snapping, anticipatory speed, a tracker reading its guide, or (later) export. Extending the span brings the frames back as they were.
- **Edits** are ordinary document edits: one undo step each, a timeline drag one gesture. A changed span re-derives what reads the entity, like any change to its output.
- **Trackers** don't spend jobs outside their span. Jobs still start at the anchor (the path depends on where it began), so an anchor before the span tracks up to it as a lead-in; nothing past the far edges is tracked. Trimmed results stay (hidden); extended again, the frames it already had come back without tracking, and frames it never had are tracked from the nearest result. The tracker's `Reach` (derived: its guide's frames in the directions it runs) is how far its ends can be dragged; dragging an end onto its reach, or onto the end of a sketch's data, untrims that side.
- **A trimmed guide** guides only where it lives: its tracker plans over the trimmed frames.
- **A trimmed view** holds its nearest framing outside its span, as outside its sketch.
- **On the timeline** every lane's ends are its span's (§14). The frames outside are drawn faint; the right-click menu has *Starts here* / *Ends here* (at the playhead) and *Untrim*.

Measured (`tt_core/tests/span.rs`, `tt_track/tests/span.rs`, the timeline's headless egui test): a sketch trimmed at both ends leaves its own signal chunk-for-chunk identical while its view re-derives over exactly the trimmed frames; a tracker spanned 400–800 around an anchor at 600 tracks exactly 401 frames, extending the end to 1000 resumes the forward job at 801 (not at the anchor) and leaves frames 400–800 bit-identical, and trimming it back to 700 and undoing starts no job and restores every frame valid; a drag on a lane's end snaps to the playhead and undoes in one step.

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

### 6.2 Job operators: trackers (as built, M6)

Some operators are too slow to evaluate inline because they read pixels. Their kind says `job()`. Evaluation then leaves their dirty frames alone, and their dependents run on whatever results exist so far (stale-while-revalidate).

A runner system in `Set::Jobs` owns them. It does four things:
- waits until the operator's inputs are evaluated (and a drag has ended);
- snapshots those inputs into a plan, and jobs (plain data sent to threads);
- writes the results as they arrive;
- reports each changed range as an `output_changed`, so dependents update chunk by chunk.

**Which way is the user's to ask** (`TrackRun`: forward, backward, both, or paused; the tracker's buttons in the Inspector and its right-click menu). New trackers wait until asked *(changed on request: "creating a tracker should probably not automatically start tracking")*. The state only gates which side's jobs may run: switching keeps every result, Pause cancels the running jobs and keeps what they produced, and asking again goes on from the last result. Tracking only backward still produces the anchor's frame (the forward side's first). While a tracker is asked to track, edits re-track it as before; paused, they only mark its results stale. It is saved, but not an undo step (undoing an edit shouldn't stop or start tracking); trackers saved before it existed track both ways. Asking again after an error (the same direction too) tries again.

**CoTracker is gentle with the graphics card** *(changed after crashes: "we shouldn't have it start automatically upon a new project")*. Each CoTracker job is a Python process that loads the model onto the card, and several at once made the card's driver reset it (Windows does that when the card stops answering for 2 s), which ended the app:
- **It never starts by itself when a project opens.** A CoTracker tracker asked to track is held paused on load (`pause_cotrackers_on_open`): a derived `PausedOnOpen` marker, never saved, makes `run_of` say Paused, while its saved direction stays in the file (no undo step, nothing to save), so every open holds it again. The top bar says how many are held, until each is asked again; the Inspector says what it was asked before. Asking any direction, Pause included, ends the hold and is saved. Template trackers (CPU) still go on.
- In catch-up mode a CoTracker side whose frames are past the playhead, with the worker taken, waits quietly ("waiting for the playhead"): it doesn't keep the app busy, and starts when the playhead moves.
- **Up to three CoTracker jobs at once**, across all trackers (`TT_COTRACKER_JOBS`, 1 to 4), as streams through **one shared worker**: one Python process with the model on the graphics card once (several processes each loading it made the card reset, 2026-10-04). The worker takes each stream's frames as they come (`O`/`F`/`E`/`X` messages tagged with the stream's number) and runs one window per ready stream in turn; each answer names its stream. Jobs hand frames to a small queue its writer thread empties, so a cancelled job never blocks on a busy worker, and a job waiting at the playhead keeps taking the answers still coming in. The worker closes after 30 s with no streams; one that stops answering for 2 minutes is stopped. Further jobs wait for their turn ("waiting for its turn" on their spinner).
- **Started early** *(on request: "auto start up cotracker engine so we don't have to warm it up the moment we place a cotracker point (but warn the user via quick popup/notif if that fails, try not to make it crash the app.)")*: once a video is open and CoTracker is set up, the app starts the shared worker in the background (`learned::warm_up`, `tt_app::cotracker::EarlyStart`) and keeps it loaded while it runs (no idle close). Before it says it is ready, the worker tracks one practice stream on blank frames (`practice`), so the slow first windows on a card (cuDNN picking its algorithms, CUDA graph capture) are done before the first real tracker. The top bar says *CoTracker is starting* while it loads; Settings > CoTracker turns it off and says its state (the device it runs on, or why it failed). If it can't start, a notice (bottom right, 20 s, *Open the doctor* / OK) says why, and the app goes on: template trackers work, and a CoTracker tracker still starts a worker when it tracks. It is tried once a run, and again after the doctor sets CoTracker up; never right after the app restarted because the graphics card stopped. Measured (`tests/cotracker_queue.rs`, the fake worker): the engine started early is ready before the first tracker, which never waits for a model and starts no other process; a worker that fails as it loads, and a Python that isn't there, each give a reason and no panic. The real worker on the CPU: ready in 6 s with the practice, 2 s without.
- **Paint trackers** (`Method::Paint`, `job::paint`) *(on request: "Paint to track as its own type of tracker: paint on frame on the thing we want to track, then we can 'reset paint' like reset points by painting on the thing again in a different frame … uses that to determine continuity and what points are kept")*: CoTracker on many points instead of one pixel. With the Track tool, hold and brush over the subject (the brush is the click's size: Ctrl+wheel); the area brushed is a *paint*, a look whose mask is what was brushed. Each paint gives up to 48 points spread over it, all queries of one stream (points in one stream cost the model almost nothing more). On every frame the points the model sees give one similarity (move, turn, scale) from the first frame, fitted by least squares and twice more without the points far from the rest (points that slid onto the background); the tracked point is the first paint's centre through it, and its box scales with it. One point: a move only; none: the last motion holds and the frame is lost. A paint on another frame is a *reset paint*: the earlier points not on it are left out from there (they left the subject), its own points join (placed through the motion on its frame), and where none of the earlier points are on it, the motion starts again from it (its centre is the tracked point there). A paint job always starts from the first paint (no resume: the motion is built from there). Measured: the fit and the reset paints on made-up model output (`job::paint` tests: outliers left out, points that slid off dropped by a reset paint, a restart where nothing is on it); end to end with the fake worker (`tests/cotracker_queue.rs`: it follows the sprite to 1.5 px, and starts again from a reset paint 40 px away); the real model on the CPU (`tests/cotracker.rs`, 61 frames of the sprite): median error 0.17 px, max 0.60 (one-pixel CoTracker on the same: 0.39, 0.66).
  - **Paints are intersections** *(changed on request: "i want the individual paints of each keyframe to act like INTERSECTIONS where only the points who make it from one painting to the next painting are considered actual trackers"; asked: every paint seeds and filters; the output stays the cohort's rigid motion)*: between two paints the cohort is the points in it at the first (or seeded by it) that land on the second; only they give the motion there, and points that don't make it count nowhere in that stretch. After the last paint the cohort is what it left. A stretch's frames come out when its end is tracked (who made it is known then); after the last paint, as they are tracked; a stream that ends first lets them out as they are. A stretch nothing makes it through holds the last motion, lost, and the motion starts again from its end's paint. *Painting*: a stroke on a frame where the selected paint tracker has a paint adds to it, anywhere on the picture (patches with gaps between them are one paint; its mask gets finer cells over a large area, 32 to 128 a side); Alt+stroke erases from it (a paint left empty goes). The crop zooms out so every paint fits. *Seeing it* (asked: dots always, paths optional): each frame's cohort points come out with the results (`PaintPoints`, not saved): on the video, a dot per point (the tracker's colour: it makes it to the next paint; faded red: it doesn't; hollow: the model doesn't see it there), and with *Settings › Paths of paint trackers' points*, the selected tracker's points' paths over the nearby frames. Measured: `job::paint` tests (three points that slide off count nowhere in their stretch and show as not kept; a stretch nothing reaches is lost, then it starts again; after the last paint, frame by frame), `tool` tests (patches far apart stay one paint without the gap; erasing), end to end with the fake worker, and the real model on the CPU (unchanged: median 0.17 px).
- **Cursor trackers** (`Method::Cursor`, `job::cursor`) *(on request: "detect multiple distinct, shared patterns/edges [of] valid mouse icons from many different images with different backgrounds … not have the user have to manually paint … paint as loosely as our current paint feature … and somehow extract the useful info from it reliably"; then: "shift + brush … for quickly adding a new shape. call them Patterns … the learned matching pattern rendered underneath the cursor as we paint … switchable with 1-0 keys … mark low confidence the same as other trackers")*: built in, no model. With the Track tool, brush loosely over the mouse cursor on a few frames; each paint teaches one *pattern* (a shape the cursor takes: the arrow, a hand), the one picked (keys 1–0, a click on its tile, or the selected paint's; else the last painted); Shift+brush starts a new one on the selected cursor tracker (not a new tracker). *Learning*, per pattern from its own paints: frames 3 before and after each paint are cut at the same spot; pixels that differ from both are where the cursor is *now* (it moved; a repeating background, a studded floor, doesn't); that cut-out (with what it walls in: on a page of the cursor's own colour only its outline changes) is found in the other paints by masked normalized cross-correlation, both ways round, the clearer kept (what moves in one may be more than the cursor: a spinner beside it the other hasn't); lining up by edges is the fallback where nothing moved (it lines up a repeating floor as well as the cursor). Lined up (to a fraction of a pixel), a pixel is the cursor where *every* paint agrees on its value *(changed on request: "white backgrounds should not be merging into the cursor … there's a painted area where the cursor is clearly not on a white background … sometimes there is a loading blue windows circle next to it but on other frames there isn't")*: the cursor is the same picture on every frame, so a pixel any paint shows otherwise is behind it (each paint with its frames near is one say, as they show the same background; with six or more paints one in six may disagree), and it is on or inside edges they agree on; a pixel that looks the same as what is behind it, on every paint whose frames near saw behind it, is that background (the same on only some paints means it can't be seen there: the cursor's white on a white page); a paint that doesn't fit what the others make is left out. Then the shapes are looked for on 12 frames around the paints, and found for sure on a new background they are learned again with it. Learning runs in a thread of its own as soon as the paints change, paused or not (`runner::learn_cursor`, `CursorShapes`), shared with the jobs (`model_key`). *Finding*: every frame searched whole (or the guide's box), each pattern coarse (its smaller side ~7 px) then exactly around the best places, masked NCC weighted by how sure each pixel is; of matches nearly as good, one that moves beats one that has stayed put 8 frames (a look-alike in the scenery), then one near where it was. Its point is the middle of the shape's top edge. Below its `min_score` (0.8 for a new cursor tracker: anywhere on the frame, only a close match counts) a frame is lost, held where it was: red on the timeline, as other trackers' bad frames. A paint change re-learns everything, so every frame re-tracks. *Seeing it*: each pattern its colour (paints on the video and the timeline), its learned shape a tile over a checkerboard (the Inspector, and under the brush while painting). Measured (`tests/cursor_paint.rs`): the cursor fixture from 13 loose paints, 96.8% of frames within 3 px of the hotspot (each pattern's constant offset taken out), median 0.11 px, look-alikes and the dimmed stretch included; on real footage against a track made by hand (`TT_CURSOR_BENCH`), 1080p Roblox: the arrow 173/174 and the hand 22/22 frames within 3 px (one clip), and over 1872 frames hand 1560/1560 and arrow 295/295, 14 frames not found (0.7%; before every paint had to agree: 328, the hand's shape carrying some of the dark floor). 30–50 frames a second at 1080p. A cursor that sits still on every painted frame (the same screen behind it) can't be told from the screen: paint it tightly.
- **TAPNext trackers** (`Method::TapNext`; experimental) *(on request: "tapnext precision tracker … whatever you think is the best way to interface with a model like this")*: Google DeepMind's TAPNext++ (arXiv 2604.10582; code and weights Apache-2.0), a point tracker that keeps a fixed-size recurrent state and answers each frame as soon as it is in (no window: catch-up mode has no 8-frame wait). It is a third point method, used exactly as CoTracker (click the pixel; reset points; the same crops and jobs), run by the same shared worker (`"method": "tapnext"` in a stream's header; `editor/tapnext/`, vendored with its license and notice; torchvision's one ViT block copied, so it needs nothing beyond PyTorch). The model loads the first time a TAPNext stream opens; a failure fails that stream only. Each query gets 64 helper points around it (one point alone lost the sprite in 2 of 4 cold starts). Its score is the visibility (it has no confidence). Weights: the doctor's *Set up TAPNext* (once CoTracker is ready) downloads DeepMind's 2.5 GB training checkpoint (SHA-256 checked), keeps the weights as fp16 (389 MB; 0.003 px from fp32 on the fixture) next to CoTracker's, and restarts the worker; or `TT_TAPNEXT_WEIGHTS`. Measured on the CPU (sprite, 512×384 crops): median 0.49 px, max 1.32 (CoTracker on the same: 0.24, 0.55); about 700 ms a frame (CoTracker: 150); a reset point mid-stream equals the offline result (9e-5 px). So it is not the more precise one here; what it may do better (long shots, finding a point again after it was hidden) is the paper's claim, not measured here, nor its speed on a graphics card.
- **The worker's code is the program's** *(found 2026-10-08)*: the doctor wrote the worker's code out once, at setup, so a newer trackertools ran the old worker (no practice window, no batching). At start, before any worker runs, the copy is compared with the program's packed code (a SHA-256 stamp in `code\stamp.txt`) and written anew if it differs (`cotracker::refresh_code`).
- A job of one frame (the anchor of a backward-only tracker) starts no worker. A worker that stops answering for 120 s while tracking fails its job. A cancelled one ends at once, and is let go of gracefully (its input closed) before it is killed.
- Dev switches for the worker: `TT_COTRACKER_GRAPHS=0` (no CUDA graphs), `TT_COTRACKER_BENCHMARK=0` (no cuDNN autotuning at start).

Dirt only says "look again". The runner compares the new plan with the one the results came from, and re-tracks from the first frame whose inputs differ, on each side of the anchor. So a stroke on a guide re-tracks from the stroke, although a sketch reports its whole extent as changed. A job whose inputs are unchanged keeps running; the others are cancelled and restarted.

Results are document data written outside edits: `History::touch` marks them for saving, with no undo step. An operator that comes back (redo, an undone delete) recomputes itself.

**The tracker** (`tt_track`) is the first such operator. Inputs:
- `guide`: a box producer, normally a sketch;
- `space`: a view.

Output: `[x, y, left, top, right, bottom, score, flags]` (flags: §6.3).

The rough pass is what makes it robust:
- the tracker only searches the guide's box;
- it predicts from the guide's motion;
- where it can't see the subject, it follows the guide.

It runs forward and backward from its anchor, with `Footprint::Radiating(anchor)`. A saved hash of its inputs, set only on complete results, lets a reopened project keep them instead of re-tracking.

Strategies (templates, and CoTracker3 in a Python worker: §6.3) sit behind the same operator and job protocol. The protocol: guide boxes and view maps per frame, a rendition, an anchor and a direction go in; result chunks come out. TAPNext or SAM 2.1 would slot in as further workers.

### 6.3 Defining a tracker: looks, the Track tool, validity

*(Built after hands-on use: "I have no idea what point it's selecting… it just appears somewhere." A tracker's point came from the sketch and its pattern from a fraction of the sketch's box, and re-centring moved it again afterwards, so the user controlled none of it.)*

A tracker is defined by what the user shows it. State stays flat, and every part stays re-tunable and removable:

| Entity | Is | Connected by |
|---|---|---|
| **Tracker** (`track` operator) | anchor frame, direction, search, adapt, min score, rendition… | inputs `guide` (a sketch: the search region and the motion prior; optional), `space` (the view it works in: by default the guide's own stabilized view), `look` (one per look, in order) |
| **Look** | a pattern the subject can look like: a frame, a rectangle there (source px), and an optional painted **mask** (which pixels are the subject) | the tracker's `look` inputs, like a sketch's strokes |

- **The first look is the seed.** Its frame is the anchor, and the tracker's point there is exactly its centre, where the user put it. Re-centring on the guide is off for placed trackers.
- **Every look is a template.** The rectangle is resampled through the tracker's view. Weights are the mask where painted, the centre-weighting where not. On each frame the best-matching look wins, blended with the last frame's appearance (`adapt`). A cursor that changes icon is several looks on one tracker. The mask keeps the background behind the cursor from counting. New looks get their mask painted automatically (the cells that stand out from the rectangle's border; a setting, on by default).
- **A match must look alike, not just correlate.** Normalized correlation sees only the shape of light and dark, so a dim patch of foliage with a similar gradient scored 0.97 against a white cursor. The score is now scaled down where the pixels' contrast differs from the look's by more than 2× either way (in proportion), and, for a painted look, where their brightness differs by more than one of its spreads: a screen recording never relights the cursor's own pixels (`ncc::photometric`).
- **How alike is alike enough** is per tracker (`Tracker::matching`, in the inspector). The defaults are the behaviour from before these options:
  - `contrast` (2): the contrast may differ by this factor either way for free, then counts less in proportion. Raise it where the whole picture dims or brightens with the subject (a pause menu's backdrop over a game's own cursor). The default stays 2× because the same check is what keeps a dim look-alike (the foliage) from scoring like the cursor.
  - `brightness` (1): a painted look's brightness may differ by this many of its spreads for free; it is gone two spreads further.
  - `colour` (off) and `colour_slack` (20 chroma levels): compare the colour too. The mean chroma (U, V) under a placement must be within the slack of the look's, and the score is gone at twice it. A white cursor and a yellow marker of the same shape are nearly twins in brightness (0.99 to each other), and colour tells them apart (0.38). Chroma comes from the decoded NV12 frame (half the resolution each way, resampled through the tracker's view like luma). It is only computed where a placement scores at least 0.4 without it, since it can only lower the score.
- **Every look is a pin.** On a look's frame the tracker is where the user showed the subject (score 1), and tracking goes on from there, in both directions. Patching a tracker where it misses is adding a look there.
- **One point for all looks.** A look's centre is wherever its rectangle happened to be, so looks disagreed by a few pixels on where the point is, and the path jumped when another look matched best. When jobs start, every look is matched on its own frame (within its half-size of where it was put) against the looks aligned before it, starting from the seed, then nearest the anchor first. Where one matches (score ≥ 0.75), the look takes on that point; a look nothing matches (another icon) keeps its own centre. Pins land on the aligned point too.
- **Search:** within 40 patch px of two predictions first: the guide's point plus the last offset, and where the tracker itself was going (its last position plus half its last step). Where nothing there reaches `min_score`, or only outside the guide's box, the whole patch. The patch covers the guide's boxes (× `search`) on the frames ±8 around this one: a rough pass is early or late where the subject starts or stops (a sketch's zero-phase smoothing moves before a flick and keeps moving after it), so a resting cursor sits outside the guide's box of its own frame but inside one a few frames away. Placements are ranked by score, less up to 0.3 for lying outside the guide's box (in proportion, up to its half-size out), so a look-alike the guide has left behind loses to the subject where it still is. The look that matched last is searched first; the others only when it scores below 0.9 (an icon change), and a window of more than 81 × 81 placements is searched coarse to fine (every other placement, then all of them around the best three).
- **Subpixel:** the correlation's peak (a parabola per axis) is refined by Lucas–Kanade: Gauss–Newton on the weighted difference to a gain and offset of the template, so it ignores brightness and contrast like the correlation (`ncc::refine`).
- **Both ways** (`fuse`, on by default): a job keeps the patches since the last pin (up to 256 MB), and on reaching the next pin tracks that stretch back from it. Per frame, the fused path takes the better pass: where they agree (within 1 view px) their mean; elsewhere the least-cost path through the two (a frame's cost: 1 − score, +1 lost, +0.5 per guide-box half-size outside the box; switching passes costs 0.6, except where they agree). The pass back stops once it has agreed with the first for 12 frames in a row. So a look placed where the tracker slipped mends the frames before it as well as after. Frames that change are sent again (progress doesn't move back).
- **Validity is a flag, never a deletion.** Output `[x, y, left, top, right, bottom, score, flags]`. `flags` marks *lost* (score below `min_score`) and *outside* (the point left the guide's box: the rough pass says the subject isn't there). Raw values stay. Consumers (views framed on the tracker, re-centring, export) skip flagged frames; the overlay and timeline draw them red. Changing the rule re-flags, it doesn't re-track.
- **Trackers with no sketch** *(built on request: "don't require a parent sketch for everything... any tracker could be under a root sketch, the entire viewport")*. A tracker made where no sketch is (the Track tool, away from every sketch's box) has no `guide` input: its guide is the whole frame on every frame of the video (`runner::plan`), so it tracks both ways over the whole clip. The template method searches a patch as big as any (±200 patch px) around where the tracker was going (its last position plus half its last step) instead of around the guide's boxes; going backward, a segment keeps its decoded frames (up to 256 MB) rather than its patches, since that patch is only known while tracking. CoTracker sees the whole frame in its 512 × 384 input, as it is usually run. Measured: a template tracker with no sketch on the sprite fixture, 1200 frames both ways in 6.0 s, median 0.076 px, max 0.18 px (as precise as with a sketch); CoTracker with no sketch, 61 frames, median 2.97 px (the 1920 px frame squeezed into 512: ~a model pixel; inside a sketch the model sees the sketch's region, closer). A sketch is still worth drawing: it is the motion prior, and flags frames where the tracker left the subject.

### 6.4 The human layer, manual dots and reset points

*(Built on request: "for any automatic matching algorithm type tracker, those trackers could be modified to allow for human overrides in case they mess up on certain frames... an additional layer above such auto results... wherever there isn't an override we fall back onto the automatic algorithm", "a very basic Manual dot whose data is literally only the human override wherever the mouse is held down", and "CoTracker... looks rephrase to reset point... one cotracker == one pixel at a time".)*

- **Two layers per tracker** (`tt_track::human`): its **automatic results** (`AutoOutput`, what the runner writes) and its **human layer** (`HumanLayer`, `[x, y]` source px on each frame a person drew its point). Its `Output`, what everything reads (views, subjects, exports, overlays), is the two composed (`human::compose`, in `Set::Jobs` after the runner, only over the chunks either layer changed): on each frame the drawn point where there is one (score 1, no flags, the automatic box's size around it), else the automatic result as it is (valid or stale), else nothing. Drawing never changes what the algorithm tracks (resuming reads the automatic layer): erase a drawn frame and its own result shows again. Undo covers both; a tracker saved before the layers existed gets its automatic layer from its output when opened.
- **The Draw tool** (`M`, "Draw by hand" in the top bar): hold on the video and every frame shown while holding takes the pointer's position (paused: the shown frame, as long as you hold; Space plays and records across frames; frames skipped while playing fast are filled in a line, a seek isn't). With a tracker selected it draws that tracker's human layer; with nothing selected (or Shift) it starts a **manual dot**. Alt+hold erases. A hold is one undo step; Esc cancels it. The Inspector says how many frames were drawn and erases this frame's or all; the right-click menu erases the shown frame's.
- **Drawing inside the view that follows it** *(fixed after hands-on use: "when my view is on a tracker, and im using a manual draw by hand, wherever i draw it weirdly keeps expanding outwards … my view should just update after that and not in the middle")*: a view following the tracker being drawn moved with each frame drawn, under the pointer, so the next frame's point landed further out, and the drawing ran away. While a Draw stroke is held, every view waits to be recomputed (`op::Held`: those operators keep their dirty frames, and what reads them waits too); when it ends they catch up and the view eases to the new framing. *(Generalised after the same run-away came back dragging a subject in its own view: "if i have it focused in viewport thus centered on it we get the same infinite expanding offset bug")*: any drag on the video, a Draw stroke, a subject or a layer, asks for the hold (`view::HoldViews`), and while it holds the view shows it: a ring where the drag started, a dashed line to the pointer, and the note "The view holds still while you drag. It catches up when you let go." Measured (`tests/subject.rs`): a subject dragged 30 px in its own view moves 30 px (without the hold, 1232 px). Measured (`tests/human.rs`): a pointer held 30 px right of the view's centre while ten frames go by lands exactly where the view showed it before the stroke on each one (without the hold, 1.7 px off by the second frame and growing).
- **A manual dot** is a tracker with only the human layer (`Method::Manual`): no algorithm, no jobs, nothing to plan; it is a tracker everywhere else (subjects, stabilizers, exports).
- **Merging dots into a tracker** *(on request: "manual dots can be merged on top of trackers by dragging them on top of it in the timeline so it applies as a kind of manual override mask data")*: drag a manual dot's lane (its name or its frames) onto a tracker's lane in the timeline, or right-click a selection of dots and one tracker: *Merge … into …* (`human::merge_dots`, one undo step). What each dot drew inside its lifetime goes into the tracker's human layer, so it overrides the tracking on those frames and the automatic results stay underneath. A later dot wins over an earlier one, and both win over what the tracker had drawn there. The dots go, with their views; whatever used a dot (a subject's member, a tracker's guide) uses the tracker instead, once. Selected dots are carried together; a drag that starts on a dot is no box selection, so a box starts in an empty part of the lanes.
- **CoTracker's reset points.** CoTracker follows one pixel at a time, so its looks are *reset points*: each says which pixel it follows from its frame on, exactly where it was put (no template alignment, which the template method's looks get). There is one a frame: placing another on that frame moves it (`set_reset_point`). The Track tool makes a point where a press is let go (no rectangle), the Inspector lists them by frame and position, and selecting one says how to move it (no mask editor: a mask means nothing to a point tracker).
- **What it is doing.** A job reports its phase (`job::Phase`): starting (decoding its first frames, cutting its looks), loading (CoTracker's Python worker starting and loading its model: seconds), tracking. With waiting at the playhead, queued (no free slot), paused, done and failed, that is a spinner (`tt_app::icons::Activity`) beside the tracker on the video, at the start of its timeline lane and in the Inspector; amber while CoTracker loads, and the top bar says "starting CoTracker: loading its model".
- Measured (`tests/human.rs`, and the timeline's own test of the drag): merged dots override the tracking on their frames (the later one on top, nothing outside a trimmed dot's lifetime), the subject keeps the tracker once, and one undo brings it all back; drawn frames over a lost, stale result come out valid with score 1 and the result's box size, the result shows again where erased, and both undo; subjects take drawn frames as good points; the Draw tool makes a manual dot in one undo step and fills two frames skipped while playing; a CoTracker's second reset point on a frame moves the first.

**The Track tool** (`T`; `T` or `Esc` leaves it). The top bar has a button for each kind of tracker it makes: *Template tracker* and *CoTracker* (`NewTrackers::method`; `T` makes the kind chosen last). CoTracker's is greyed out, saying what's missing, where its Python worker, a Python or its weights aren't found (a copy given to someone without the repository's Python setup):
- **drag** a rectangle on the video: a new tracker with that look on the shown frame;
- **click**: a point tracker with the brush-sized pattern (a dashed box shows it, 28 pt by default; Ctrl+wheel sizes it, the plain wheel zooms as always);
- with a **tracker selected**, a drag or click **patches** it: another look, on this frame, where the subject really is (the path is pinned there; a new icon is learned too). `Shift` makes a new tracker instead. *(Changed after hands-on use: "my workflow is to just kinda patch wherever it seems to miss".)*
- the guide is the selected sketch (or the selected tracker's guide), else the smallest sketch whose box holds the rectangle's centre on this frame, else none: the tracker searches the whole frame (§6.3, trackers with no sketch);
- a **CoTracker** follows a pixel: a press makes a point where it is let go (a new CoTracker, or with one selected its reset point on that frame, moved if it has one there; §6.4);
- the tracker works in the guide's own view (created if needed), so the pattern is cut and matched where the subject sits still.

**The Look editor** (a panel): the selected look's pixels, magnified. Paint the mask (left paints the subject, right erases; a drag is one undo step); *Auto* (the cells that differ from the rectangle's border: a cursor on a plain background), *Fill*, *Invert*, *Clear* (back to centre-weighting). Editing a look re-tracks.

Also: guide-seeded trackers (*Track its centre* on a sketch, `T`) keep re-centring on the guide. *Re-seed here* (`T` on a tracker) starts it again from its look on the playhead's frame (a painted one first), which moves first. With no look there, it switches to the Track tool and the next drag is that look. *(Fixed after hands-on use: re-seeding used to make a look wherever the tracker was on that frame. Where the tracker had drifted onto dark foliage, that look was foliage, and it became the seed, so the tracker followed foliage at 0.97.)* Deleting or restoring a look re-tracks (any input without an output of its own recomputes its readers when it is deleted or restored).

**Re-tracking only what a look changed** *(on request: "limit recomputation from when paints/reset points have actually changed, and their immediate neighbors only. and show in the timeline an animating bar so I know which regions are being recomputed/treated as stale")*: a look that changed, came or went no longer re-tracks the whole tracker. Results hold except from the look before it to the look after it, on its side of the anchor (the anchor's own look: both sides) (`runner::Plan::touched`); the guide and view rule is as before. A paint tracker's motion builds on the stretches before, so from the paint before the changed one to the end of that side; its job resumes on that paint from the state it left there (`PaintStates`: the motion and the cohort, each point's place in the reference and where it was; kept where results hold, not saved), not from its first paint. A job tracks one stretch to redo and stops; the next stretch gets the next job. A template tracker's looks are matched on every frame, so a change there could affect frames further away; it is limited to the neighbours all the same, as asked. On the timeline, a tracker's stale frames (to be tracked again, or being) have stripes moving along the bottom of its lane. Measured (`tests/cotracker_queue.rs`): a reset point added between two others re-tracks only between them, in one job, the rest valid throughout; a paint moved re-tracks from the paint before it, the earlier frames valid throughout, and ends where a fresh tracker with the same paints does (0.5 px).

**Switching a tracker off** (§6.5; `tt_track::off`) *(on request: "enable or disable a tracker easily … marked as disabled if they go off the rails and then can come back again when you know they're good, and at that point other trackers would contribute/take over if they are enabled. hotkeys for that too? visual indication in timeline as well")*: a tracker's off keys (saved, undone) say from which frames on it is off, and on again. Where it is off its output carries the `OFF` flag (`human::compose`, over its tracked and drawn frames alike), so what reads flags leaves it out as it does lost frames: a subject goes on with its other members, exports and stabilizers without its points. Its results stay; switched on, they count again. `H` / `Shift+H`, the Inspector (*Switch off from here*, *Switch on from here*, and where it is off), and the right-click menu switch it; the timeline hatches its lane where it is off (and doesn't count those frames as lost), and on the video it is grey, labelled *off*. Measured (`tests/off.rs`): a subject of a slow and a fast dot moves with the slow one alone while the fast one is off, and with both again after; the fast one's point is where it was; undo.

**The right-click menu's trackers** *(on request: "if I have a tracker (or multiple) selected I can right click on the timeline and it has options to Track (N) trackers forward, back or both from the time cursor … or if any are tracking say Pause (N) running trackers")*: at the top of the entity menu (timeline, outliner, viewport): *Track N trackers forward / backward / both ways from frame F* (`track_from`: one with a look, reset point or paint on F starts again there, as *Re-seed here*, one undo step; the others go on from their anchor and results), *Pause N running* (the selected ones running, or with no tracker selected, every running one), and switching them off or on.

**On real footage** (the user's 1080p60 gameplay, their Tracker 1: 5 painted looks, frames 37619–40235, replayed headless by `tests/real_footage.rs`; "on the cursor" = at least 12 near-white pixels within 14 px of the point):

| | on the cursor | longest miss | speed |
|---|---|---|---|
| the sketch alone (the guide) | 78.8% | 48 frames | |
| the tracker before these changes (its saved result) | 84.6% | 160 frames | |
| after, with its 3 re-seed looks of foliage still in | 99.6% | 9 frames (at those looks) | 72 fps |
| after, those 3 looks deleted | 100.0% (1 frame off) | 1 frame | 93 fps |

Frame to frame, the point's offset from the cursor's white body changes by a median of 0.30 px (p95 1.08). Before the changes it was 0.36 px (p95 1.43), and that was only on the frames where the tracker was on the cursor at all.

**The cursor fixture** (`cargo xtask fixtures` renders it; `tests/cursor.rs`): 750 frames of 960×540 H.264 (GOP 250, B-frames) of a cursor with its exact hotspot per frame. It has five stretches: stripes that change every frame, pale bright scenery, dark foliage where the cursor turns into a hand and an I-beam, a floor it flicks across (3–5 frames, up to ~190 px per frame), and a desktop with a second, identical arrow the cursor rests on and flicks away from. The guide is a sketch-like rough pass (the path smoothed with σ 4 frames, a slow wander of a few px, a box growing with speed); the looks are masked rectangles on frame 5 and on a frame of each other icon.

| | changing | bright | icons | flicks | look-alike | all: median / within 3 px | speed |
|---|---|---|---|---|---|---|---|
| before | 100% | 100% | 99.3% (max 8.2 px) | **82.0%** (max 179 px) | 96.0% | 0.14 px / 95.5% | 52 fps |
| after | 100% | 100% | 100% | **100%** (max 0.20 px) | 97.3% | 0.08 px / 99.5% | ~90 fps |
| after, a look at 648 where it slipped (one way / both ways) | | | | | 97.3% / 98.0% | | 85 / 75 fps |

**Matching options** on the same fixture, extended by two stretches: bright scenery with a static *yellow* arrow the cursor rests on and flicks away from, and a floor dimmed to 30% (cursor included) for 90 frames. These are the settings `tests/cursor.rs` compares, with no stretch worse than the default:

| | colour (yellow twin) | dimmed | speed |
|---|---|---|---|
| default | 99.3% (one frame on the twin, 153 px off) | 62.0% (all 90 dimmed frames lost) | ~90 fps |
| `colour` on (slack 20) | **100%** | 62.0% | ~10% slower |
| `contrast` 3 | 99.3% | **100%** | same |

Also measured: a colour slack of 12 caught the twin too, but lost the cursor over a saturated blue panel (4:2:0 chroma blurs the surroundings into a 12 px cursor's colour): the look-alike stretch fell from 97.3% to 92.0%. `brightness` 2 changed nothing. An *edges* channel (the correlation of luma gradient magnitudes, mixed in) helped on no stretch, flagged more frames and cost 2.5× the time, so it isn't offered.

(Within 3 px of the point the look's centre defines; the misses left are three frames where the cursor flicks off the identical arrow at ~110 px per frame, which fool both passes. Speeds are from shared cloud machines.)

Measured (`tests/sprite.rs`, `tests/masks.rs`, `tests/template.rs`, the in-app demo):
- after the changes above (before → after): a placed look on the sprite, median 0.080 → 0.072 px (max 0.187 → 0.198); a sketch-built guide, median 0.319 → 0.284 px (max 0.785 → 0.897); both ways from a guide point and re-centred, median 0.211 → 0.187 px (max 0.560 → 0.419); synthetic subpixel motion (a blob), median 0.016–0.061 → 0.007–0.012 px. The sprite sits on whole pixels, as a screen cursor does. Refining through a matched blur (to remove bilinear resampling's pull toward whole pixels) was exact on an ideal square between pixels but worse here (0.100–0.109 px for the placed look), so it isn't used;
- (before the changes above) a placed look on the sprite fixture: median 0.08 px, max 0.20 against the truth, no re-centring;
- the demo's Track tool (a 24 px square dragged on frame 340, inside a sketch ~2.6 px off): median 0.08 px, max 0.21;
- a masked, antialiased cursor arrow crossing a changing background: worst score 0.77, error ≤ 0.53 px (unmasked, the score drops to 0.43: lost);
- frames outside the guide are flagged, and their raw positions kept. With the wider search, a sprite 30 or 36 px outside a wrong guide's box is kept (flagged *outside*); 120 px off, it is lost.

**Learned point trackers** sit behind the same entities as another `method` (`Tracker::method`: *Template* or *CoTracker*). CoTracker3 (v1's original tracker type; Meta's `scaled_online.pth`, CC-BY-NC, not in the repository) is built:
- The job is the same one: the runner, the plan, spans, catch-up, the looks and their alignment, pins, the output and its flags are shared. Only the per-frame work differs (`job/learned.rs`).
- **Frames in Rust.** The job decodes as always and resamples each frame through the tracker's view into the model's input: a 512 × 384 RGB crop (luma and the NV12 chroma, BT.709 for HD, BT.601 below, limited range). The crop has one scale for the job, so the guide's box (× `search`, the largest on the job's frames) fits with a margin, and is centred on the guide's point on every frame. The rough pass stabilizes what the model sees.
- **Direction is the frame source's.** A backward job decodes keyframe-aligned segments and sends them reversed; the model only ever runs forward.
- **Seeds** are the looks' aligned points, each queried on its own frame; the start is the anchor's look, or where the tracker was when a job resumes. On each frame the latest seed behind it answers (a fresher seed has drifted less), and a look's own frame is pinned where the user put it. The score is the model's visibility × confidence; below `min_score` a frame is flagged lost, keeping the model's estimate.
- **The model runs in a Python worker** (`editor/cotracker_worker.py`, one process per job). It reuses v1's online engine (`editor/engine.py`: the rolling window, CUDA graphs on a GPU) and speaks JSON lines plus raw frames over stdin/stdout. Python is `TT_PYTHON`, else the environment the doctor set up (below), else the repository's `.venv` (as v1 set it up), else `python3`/`python`; the worker script likewise. The weights are `TT_COTRACKER_WEIGHTS`, else the doctor's download, else torch hub's cache, where v1 downloaded them; the worker is told which (`--weights`).
- **On a computer without the repository, the doctor sets it up** (`tt_app::cotracker`; Settings → Doctor, the CoTracker button when it isn't set up, and step 5 of *JUST DO EVERYTHING FOR ME PLZ* on an NVIDIA card, going on in the background after the app starts). It reads the card from `nvidia-smi`, says what it needs (the card, a driver, about 2.5 GB to download, about 6 GB of disk, 5–30 minutes, the model's non-commercial license). If Windows' Visual C++ runtime is missing or older than 14.42, it first runs Microsoft's installer for it (`aka.ms/vc14`, checked: signed by Microsoft; Windows asks for permission, and *JUST DO EVERYTHING* waits for that before the app starts): PyTorch 2.14.0's DLLs are linked with MSVC 14.42 and load `msvcp140.dll` and `msvcp140_atomic_wait.dll` from Windows, and uv's Python doesn't bring them. Then it puts into `cotracker\` in the data folder: uv (Astral's single-file Python manager, checked against its SHA-256; no admin rights, nothing changed elsewhere), a Python 3.12 of uv's own, an environment with PyTorch 2.14.0 and NumPy, PyAV and OpenCV at the versions the worker was developed with, the worker's code (packed into the program at build time, `tt_app/build.rs`), and the model (checked against Hugging Face's SHA-256). Then it starts the worker once and waits for it to load the model on CUDA. Blackwell cards (RTX 50, compute capability 12.0) get PyTorch built for CUDA 13.0 (driver 580 or later): the CUDA 12.6 build the repository's `.venv` uses has no kernels for them, and PyTorch 2.14 isn't built for 12.8. Older cards get the CUDA 12.6 build. uv's download cache is removed afterwards.
- Catch-up works, but the model finalizes frames half a window (8) at a time, so the last few before the playhead wait for more.
- Measured on a CPU (the cloud; the user's RTX 4090 is far faster): the sprite fixture tracked both ways over frames 590–650 from a look at 600, through a guide ~5 px off, median 0.26 px, max 0.99, none flagged, at ~1.2 fps including two model loads (`tests/cotracker.rs`); a blob in the worker alone, median 0.58 px (`editor/tests/test_cotracker_worker.py`). Both skip without torch or the weights.
- **On the cursor fixture it is no match for the templates** (`TT_COTRACKER=1`, CPU, ~1 fps). Changing stripes and bright scenery: 100% within 3 px but a median of 0.7–1.2 px (templates 0.08); icons 73%; from the flicks on, 0–2%, and it never comes back. Two reasons. The crop's one scale per job comes from the largest guide box, so on a flick's big box the 12 px cursor shrinks to a few model pixels. And a point tracker never re-detects: only a look re-seeds it. Next: a scale per stretch, and letting the template method re-seed it where it loses the subject (a Target combining both, M7).

**Export: two trackers as a Resolve stabilizer** (`tt_track::export`, pulled forward from M8 on request). Right-click with exactly two trackers selected → *Copy Resolve stabilizer (Fusion)*.
- **What it makes.** A Fusion `Transform` tool as `.setting` text on the clipboard (also saved under the data folder's `exports/`), with three curves (the centre's x and y, the angle) keyed linearly on every source frame where both trackers are good (not lost, not outside, inside their spans). Elsewhere Fusion interpolates.
- **The math.** Per frame, the two points give a midpoint and the angle of the line through them. Each key turns the frame about its centre by the reference angle minus this frame's, then moves the centre so the midpoint lands on the reference's. Both points therefore stay where they are on the reference frame: position and rotation held, size not.
  - The reference frame is the playhead's when the menu is used (else the first frame with both points).
  - The math is in pixels, y up. Only the result is normalized, as Fusion's `Center` is: 0–1, y up. `Angle` is degrees counter-clockwise, unwrapped so neighbouring keys never jump 360°.
- **Frames.** The keys sit on source frame numbers and the Transform finds its own: `Center` and `Angle` are expressions reading the curves at `SourceFrame`, an expression too (`export::SOURCE_FRAME`). Measured in Resolve 21 (free) with frame-numbered clips: a clip opened on the Fusion page gets a comp at the clip's own rate (a 50 fps clip on a 60 fps timeline: a 50 fps comp) whose frame 0 is the clip's first frame in the edit (trimmed to start on source frame 37: `comp.GlobalStart` = −37), so `time − comp.GlobalStart` is the source frame whatever the trim. A Fusion Clip's comp runs at the timeline's rate from its own start and shows `start + floor(t × source fps / timeline fps)`; nothing in it says what `start` is, so the node has *Clip Starts At Source Frame* (−1 = from the trim) to type it in. The first version keyed `Center` and `Angle` straight on source frame numbers, on the belief that a trim only moved the render range: its keys landed off by the trim, and in the Fusion Clip it was first pasted into (frames 0–1056, keys 2104–2942) all past the end, holding one value for the whole clip.
- **In Resolve.** Fusion page → select MediaIn1 → paste: the Transform wires itself in between MediaIn1 and MediaOut1 (checked). The clip must be the file the trackers ran on or another encode of it with the same frames and aspect ratio (the keys are normalized; another rate is converted by time).
- **Measured** (`tests/stabilize.rs`): an ffmpeg clip of a still scene, with a camera drifting up to 103 px and rolling up to 5.7°, at 30 and at 50 fps. Two points ~380 px apart were tracked by the template tracker, and the exported keys were applied as Fusion's Transform applies them. Every point of the scene stays put: medians of 0.08–0.16 px, a max of 0.45 px, including a third point that wasn't tracked.
- **Measured in Resolve** (`scripts/resolve_stabilizer_proof.py`: through the davinci-resolve-mcp in-app bridge it pastes those exports as a person does, renders, and finds 45 patches of one stabilized frame on every other frame). Raw, the scene moves up to 150 px and turns up to 11° against the reference frame. Stabilized: untrimmed at 30 fps, a median of 0.24 px, a max of 0.68 px and 0.074°; trimmed to start on frame 25, the same; a 50 fps clip trimmed to frame 30 on a 60 fps timeline, 0.34 / 0.77 px and 0.084°, and the same inside a Fusion Clip with its start typed in. Two controls fail as they should (more than 40 px off, 1.5° turned): the first version's export on a trimmed clip, and a Fusion Clip whose start was left at −1.
- **Or rendered here** (*Export stabilized video…*, *Export tracking target video…*; `tt_media::render`, the window in `panels/export.rs`). An editor's own render can't always be trusted with the node: in a Fusion Clip on a 60 fps timeline, Render in Place and final renders broke it (its expressions read the comp's frame and rate, which those renders set differently). So trackertools renders the video itself, from the original: ffmpeg decodes it to 4:4:4 (16 bits a sample for deeper sources), each frame is warped here (bilinear, sub-pixel; outside, black as the source codes it; optionally zoomed just enough that no frame shows an edge), and ffmpeg encodes ProRes 422 HQ, DNxHR HQX or H.264 with the source's colour tags and sound. One output frame per grid frame, so it drops into any editor 1:1: over the in and out points if they are marked (the window offers *In to out* or *Whole video*, and follows the marks while open), output frame 0 being the in point's frame; decoding starts exactly there (the same seek as playback, open-GOP leading frames included) and the sound is cut to the same frames. The zoom that hides the edges is worked out for just the frames exported. Measured on the synthetic clip (real tracking, through a subject): the rendered video's scene stays within 0.39 px (median 0.21) and 0.021° of the reference frame, against 146 px and 11° unstabilized; every frame's middle within 1.1 grey levels of the reference frame's. The tracking target, for moving things with the subject in any editor, is a white-bordered checker on black (its crossing is what trackers lock onto) with a half-size partner on its own x axis (for two-point or planar trackers' rotation), following the subject's point and turn.
- **Rotation, and where it holds the subject** *(on request: "add the rotation data as optionally included… if not, only stabilizes position, as a checkmark", and "with zoom to hide the black edges I don't see the subject centred… subject must be centered in stabilized video")*. Two options, in the export window and the right-click menu, remembered (`StabilizerDefaults::rotation`, `centre`; both on by default), recomputed live in the window: *Undo its turning too* (off: the fit is the shift alone, a subject's angle is left out: the picture moves but never turns; a follower doesn't turn), and *Keep it in the middle of the picture* (each key puts the anchor, the points' centre on the reference frame or the subject's point, at the picture's centre instead of where it was on the reference frame; off, the classic stabilizer). The zoom that hides the black edges is worked out for the choice, so zoomed in, the subject stays in the middle instead of being pushed off by a zoom about the centre. Measured (`export.rs` tests): position only, no key turns and the points' centre holds still; centred, it sits on the picture's centre on every frame, still turned as on the reference frame; a subject's path too. *Show in folder* now selects the file: Explorer reads `/select,"<path>"` itself, and a path with a space given as one quoted argument made it open Documents instead (`tt_app::files::reveal`, with a test of the command line).
- **Zoom, position and a preview** *(on request: "in the export video i want a preview of what the video cropping and size will be. gimme a slider for zoom, perhaps a way to offset position as well… perhaps with a toggleable guidelines (crosshairs/lines)")*. A rendered stabilized video is framed (`export::Framing`): zoomed in about the picture's centre, then moved by an offset (a part of the width and height, x right, y down). *Zoom* is a checkbox for the least zoom that hides the black edges (as before, now worked out with the offset too, up to ×4, the slider's most; when even ×4 leaves an edge on some frame, the label says so in amber instead of claiming them hidden) and a slider (×1 to ×4) that shows the zoom in use; moving it sets a zoom of your own. The search takes a pass over the exported frames for each of its ~40 steps, so it runs on the UI thread only when the option is on, once per change of the frames, the offset or the keys, and not while the pointer is held: during a drag the last value serves, and it is worked out again when the drag ends. The sliders clamp only what is typed or dragged (SliderClamping::Edits): with the default they round the value they show and write it back every frame, which turned the option off by itself and made a dragged picture lag the pointer. *Position* is two sliders (±50%), *Reset*, or a drag in the preview. All of it is remembered with the other stabilizer options (`StabilizerDefaults::fill`, `zoom`, `offset`; the session's settings). The export and the preview share one map, `export::rendered_map` (the key on that frame through `framed_map`), so the preview is the export up to its resolution: the window takes the decoded frame nearest the playhead from the player's cache (the proxy's if it has it), draws it on the CPU through that map into a small RGB texture (`tt_media::render::nv12_preview`: four nearest samples a pixel, the viewport's colour conversion; black outside), and makes it again only when the frame, the keys or the framing change. Over it, *Guidelines* (a centre cross and the thirds) and *Point* (a ring on what the stabilizer holds, through the inverted map: the subject's point, or the points' anchor as measured on that frame, Stabilization::followed, which stays right when a tracker is missing there, unlike the mean of those left; with several points, a dot on each). It follows the playhead, and a frame slider under it moves the playhead over the frames to export. Measured (`export.rs` tests): with no offset the map and the zoom that hides the edges are the earlier ones to the bit; an offset moves the picture by exactly that much; a zoom is about the centre and as chosen; centred, the subject lands on the centre plus the offset at any zoom; a still picture moved a tenth of its width needs ×1.2, moved half its width ×2 (and with less allowed, the edges show and it says so); with three points and one of them lost on some frames, `followed` is the shape's centre on every frame and the export puts it on the centre plus the offset. The Fusion copies are not framed (Fusion's own Transform does that).
- **The follower** (the other menu entry): the same measured motion applied instead of undone: a Merge whose `Center` (and `Angle`, with two or more points) reads the curves the same way, its foreground a Text+ to start with. Pasted on the clip, text moves (and turns) with what was tracked over footage left as it is; anything can take the Text+'s place.
- **Any number of trackers, sketches too, and a spring.** Anything with a point a frame counts: a sketch's point is the hand's path (on a 4K clip, a refined sketch of a fly walking 900 px stayed on its body within 5 px, median, by an independent finder). One alone holds its point still in position only. With two or more selected, each frame's motion is the least-squares rotation + shift of all their points against the reference frame; a tracker missing there is placed by the others. The angle is only as precise as the trackers are far apart: on real footage (a 4K 360-camera clip, two points 90 px apart on a trail camera) ~0.15 px of tracking noise made it jump 0.17° (std) frame to frame, against 0.06° (p95) of real roll measured in the video, so the stabilized render gained a wobble (0.35° p95 a frame). So the measured motion can go through a critically damped spring, run forwards and backwards (no lag; a steady drift passes exactly), rotation and position apart (`StabilizerDefaults`: 0.05 s on rotation, none on position; in the menu, remembered). On that footage 0.05 s cut the wobble to 0.11°; in `export.rs`'s test, four trackers spread out cut it 20× without any.

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
| **Subject** (target) | a `subject` operator over `member` inputs (trackers, sketches, any points): their pushed motion plus its own offset keys (`tt_core::subject`, built) | the final coordinate and angle |
| **Modifier** | any Signal→Signal operator: smooth, offset (keys), lag, wiggle, influence blend | a new signal version, stackable |
| **Job** | an operator output being materialized over a range, in a direction | chunks, progress, cancellation |

**Subjects (built).** A subject is not the mean of its members' points, which would jump whenever one came, went or got lost. It starts at their mean on its anchor frame (where it was made) and is *pushed* from there, both ways, frame by frame, by the members good on both neighbouring frames: their mean shift and, with two or more, their turn about their centre (a least-squares rigid fit). A frame none of them moves holds it. While the same members carry it the steps add up exactly to their motion (no drift); where they change, an offset key puts it right. Its own transform is offset keys (position and angle, in its anchor frame's pixels so an offset turns with the thing; linear between keys, held beyond): dragging it in the Select tool keys it on the shown frame, the Inspector edits the key there. Its output is `[x, y, box, angle, flags, pushed x, pushed y, pushed angle]`, so it is also a box producer; the Resolve stabilizer and follower copy a selected subject's final data. Members are linked as inputs, so their new results re-evaluate it. (From v1's pushed position; v1's finetune layers and motion-outlier rejection are not carried over yet.)

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
  - **paused:** the hold is a **retake** of that frame: where the mouse *should* have been there (§8.3);
  - **playing:** tap `Space` while holding and it records across frames, at the playback rate (the capture speed, set with `[` / `]`; slow motion is just a rate). Tap again to pause and keep shaping that frame;
  - steps, jumps and scrubs while holding are recorded too.

  The ClockMap records every transport change, so all of it maps back to video frames.
- **New sketch:** the `＋ New sketch` button (next to the Sketch tool), `Shift`+hold, a click on empty video, or `Alt+A` (deselect) all make the next stroke start a new sketch instead of editing the selected one.
- **Strokes and sketches:** each press → release is a *stroke* (a Capture entity plus a `Stroke { falloff, influence, size, scale, lag }` component). It goes onto the **selected sketch**, so editing a rough path means: select it (a click on its box, its timeline lane, or the outliner), go to a frame, press and hold or drag. With nothing selected, or with `Shift` held at the press, the stroke starts a new sketch. `Alt+A` deselects.
- **Clicks select, holds record:** a press shorter than 0.18 s that moves less than 4 screen points is a click, in any tool. It selects the sketch whose box is under it (the smallest where boxes overlap), or clears the selection on empty video, and never records. Only a hold or a drag edits, so a stray click can't change a path.
- **Move only:** `Ctrl` at the press makes the stroke keep the box size that was there (`size = 0`). Without it, a hold also sets the size from its jiggle (§8.3), so a quiet hold makes the box tight. Both stay editable per stroke.
- **Layering (retakes by default):** a sketch's strokes are laid over each other in order:
  - frames a stroke visited take its value (blended by `influence`, like an NLA strip). Nothing else moves. The region around them re-derives from the new data (the motion union takes the retake in), so the box jumps and grows to include it, as if the recording had been that way. *(Changed after hands-on use: the falloff used to default to 0.2 s and dragged neighbouring frames' positions, which read as "ruining" them.)*
  - with a `falloff` (optional, 0 by default), frames within it of a visited run keep their own motion but move by the run's edge offset, with Blender's smooth falloff curve. Where several edits reach one frame, the weights are normalised, so the frames between two edits with the same offset move by exactly that offset (no overshoot);
  - where there is no path yet, a gap of at most twice the falloff between the stroke and another value is bridged linearly, so a path can be blocked out with holds on key frames;
  - a stroke's **size** (a multiplier on its region), falloff and lag are set before drawing, in the **Brush** tab (§14). While holding, the mouse wheel does nothing by default, so the view holds still under the hand (a stray scroll used to zoom it mid-stroke). As a setting, the wheel while holding can zoom, or set the size, the falloff or both. Every stroke stays re-tunable, and removing one restores what was under it.
- **Live feedback:**
  - the raw hand trail;
  - the sketch with the stroke laid over it (path and region), computed by the same pipeline over the samples so far;
  - the other sketches, faint;
  - on the timeline, the frames the stroke visits and the frames its falloff moves.

  Before a press, a **preview box** at the pointer, dashed and light (orange): the box a hold there would get right now, sized by the hand's jiggle over the last moments exactly as a held frame is (`sketch::hold_box`, from the pointer's last second, `tool::PointerTrail`), with the next stroke's size and falloff written under it, so moving or steadying the hand shows what it does before you press; on the timeline, the selected sketch's lane shows, dashed, how far that falloff would reach from the playhead. Once pressed, the box being recorded is solid and thick (amber: live). *(On request: "a preview box (dashed, light) to visualize how the falloff and size react to mouse movement, and then when we click it's solid and thick".)* The box outline follows After Effects' "Show Wireframe". So that nothing hides a small target at the pointer while holding, the OS pointer is hidden over the viewport, and a **clear window** (radius 24 pt) around the stroke's latest sample shows the raw video: the frame is painted a second time after the overlays and HUD, with the same view transform, through a circular mask (soft 1.5 px edge, the rest discarded), with a thin ring at its edge. Both are settings (0 pt turns the window off).
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
| **Lag** | `lag` 0.25 s wall, **per stroke** (`Stroke::lag`: a new stroke takes its sketch's `lag`, and each stays editable). Being real time, it spans 2× the video frames at 2× playback and ½ at ½×; later an optional auto-estimate by cross-correlating against a tracker | v1 |
| **Smooth (centre)** | **Steadiness** (One Euro `min_cutoff`, Hz) and **Responsiveness** (`beta`), tuned in that order; **Dead zone** (px, in the drawn-in view's space; ignores tremor); offline refinement is zero-phase (forward–backward), so it is lag-free and reverse-symmetric | One Euro filter (Casiez's tuning procedure), Blender lazy mouse, SciPy `filtfilt` |
| **Jiggle → extent** | RMS spread of the hand around a *slow, non-adaptive* reference (the steadiness cutoff alone), window σ 0.25 s; **Gain** 1.0; **Pad** 12 px; **Min half-size** 16 px. *(M3: measuring against the responsive point path under-read a jiggle, by a different amount while paused than while playing, so an edit made while paused came out 2–3× smaller. Against the slow reference the same jiggle reads the same in both.)* Still to do: a critically damped spring for grow/shrink | v1 synthesis, Screen Studio-style springs |
| **Resample** | how multiple samples on one frame (pauses, re-scrubs) combine: *last pass wins* (default) or *average* | ClockMap (§3) |
| **Union** | include motion over [f − 0.1 s, f + 0.15 s], applied to the whole sketch *after* its strokes are layered, so a frame edited while paused picks up the path's motion from its neighbours like a recorded frame | v1 |

Every stage's output signal is inspectable: raw, lag-shifted, smoothed, extent. The Smoothing panel shows raw vs smoothed trails live while you drag a slider. Presets (*Tight / Default / Loose*) sit over the numbers, following Premiere's Auto Reframe presets.

### 8.3 Hold-to-simulate while paused

1. With the video paused and the Sketch tool active, **press and hold** on the subject.
2. The ClockMap records a held segment on the current frame.
3. Samples keep streaming in wall time. The box at this frame is sized by the jiggle *during the hold* (the raw hand's spread around where it sits, over its last `jiggle_window`), so it grows while you jiggle and settles tight when you hold still. The same jiggle gives the same size as it would while playing. On an existing sketch the frame also takes the motion union from its neighbours, and theirs take it in, so a retake reads like a recorded frame.
4. The frame is a **retake**: it takes where the hand *sat* on it, the median of the raw hand over its last 0.15 s there (no lag shift, no dead zone). Neither the move that brought the hand there nor the move after leaving pulls it. *(Fixed after hands-on use: it took the zero-phase smoothed hand at the moment the frame was left, half blended with the move to the next frame, so stepping while holding wrote only about half of each position.)* Held frames are not smoothed with their neighbours: each was placed by hand.
5. It combines with stepping and playing: keep holding, press an arrow key to step a frame (or Space to play), and keep going. Each frame shown while you hold is retaken when you leave it or let go, whether frame by frame or in straight playback.

Measured (tests/retake.rs, and the in-app demo): a paused hold 60 px off lands within 1.5 px of the mouse; stepping while holding with +60 / +30 / −20 px lands each frame on its own spot (the demo's three frames 30 px right of the sprite: +30.2, +30.2, +29.9), and the neighbouring frames keep their points exactly.

### 8.4 Anticipatory speed

*(The user's idea, after hands-on use: a box that suddenly grows after being small for a while foretells erratic motion; one that stays small and still means the subject is still and can be sped through. Simplified after more use: "I don't like tweaking how anticipatory speed works… analyze how normally jittery the mouse is… the 20th to 80th percentiles… a range slider… maps how jiggly you were going to be in the next n seconds to that speed.")* On by default (the Brush tab and Settings switch it). While a stroke records **and** the video plays, the playback rate is set for you, continuously (`tt_core::autospeed`). It needs no numbers: it calibrates itself on the sketch it reads.

- **What it reads:** by default the **parent** sketch, the box the view you're drawing in frames. Whoever drew it already recorded how hard the subject was to follow: its box grew where the hand jiggled or raced. On the source it reads the sketch being edited. As an advanced setting (`foresight`): Parent / Editing / Both (the busier).
- **Calibration:** the 20th percentile of that sketch's box sizes (over all its frames) is *calm*, the 80th *busy*. It is recomputed when the sketch changes. A sketch that is about the same size everywhere still needs a clear step up to read as busy: the spread is at least half the calm size.
- **Busyness ahead:** the biggest box in the next `look_ahead` seconds of video, placed between calm (0) and busy (1).
- **…and against how it has been lately** *(added on request: "the sensitivity should be not just the entire trajectory but a sliding window of the past few seconds, so the sensitivity itself is adaptive… if it slightly moves some pixels, that must be prepared for some seconds in advance")*: what is ahead is also compared with the last `memory` seconds (3) of the same sketch. Two readings: the point's speed over a few frames (px/s, `still` 6 px/s added to both so the sketch's own wobble isn't a move) and the box's size, each against its 20th percentile there; up to 1.5× is ordinary, `sensitivity`× (3) or more is busy, log in between. The busiest of the readings (the whole sketch's, these two) sets the rate. So a subject that stood still for seconds plays fast, and a few pixels' move ahead slows it before it arrives, though its box never grows; a steady motion the hand already follows doesn't count as busy, and an erratic stretch still reads busy against the whole sketch. Measured (`tests/autospeed.rs`): standing still ×1.56; five frames before a 3 px move (box unchanged), ×0.10. The Settings tab has the sensitivity and memory (and, advanced, the still speed), and while it drives, what it reads now (a meter).
- **Speed:** busyness picks a rate in the user's range, `fastest` at 0 and `slowest` at 1, even on a log scale in between (the middle is the geometric mean). So playback slows `look_ahead` before a busy stretch and speeds through calm ones.
- **Q / E while it drives** multiply what it picks (×1.5 per press) instead of taking the rate back. The multiplier stays until changed, also in later strokes; Settings shows it with a Reset. Any other rate change while it drives (the speed menu) also becomes the multiplier. *(Changed after hands-on use: Q/E used to hand the rate back to you until the release.)*
- **Your hand** (advanced, off by default): the older reactive limits also apply, and they drive the rate where there is nothing to read ahead.
  - The hand's velocity over the last 0.1 s (a least-squares line, in screen points), divided by the rate it was seeing `lag` earlier, is the subject's on-screen speed at 1×; the rate that keeps it at `comfort` is the limit.
  - `comfort` needs no guessing: **Calibrate from my recent strokes** sets it to the 80th percentile of how fast the hand moved (on screen, at 1×) over roughly the last minute of recording.
  - Jiggle growing past its calm size slows it further (weighted by `jiggle`).
- The rate follows the target exponentially in log-rate: within `slow_down` going down, `speed_up` going up. The release (commit or cancel) restores the rate you had set.
- **The meter** *(on request: "see the parent sketch's erraticness meter … as a visual meter directly correlating to auto playback speed")*: under the badge while auto speed reads a sketch ahead: "Sketch 1 ahead: 72% erratic", a bar filled from calm (cyan) to busy (red), and a mark on the same scale where the speed is now (calm plays at the fastest, busy at the slowest, log in between), with both ends' speeds. `AutoSpeedState::reading` says which sketch it read.
- The badge reads `auto ×0.35 · busy ahead · ×1.5 yours` (or `calm ahead`, `your hand`, `nothing to read ahead`). Automatic changes never flash; a Q/E multiplier change flashes as `auto ×1.50`.
- The ClockMap records every frame's playhead against wall time, so the pipeline needs nothing for a varying rate.

| Knob | Default | Where | Meaning |
|---|---|---|---|
| `slowest` / `fastest` | ×0.1 / ×2 | Settings | busy stretches play at `slowest`, calm ones at `fastest` |
| `look_ahead` | 1 s | Settings | how far ahead it reads (video time) |
| `sensitivity` | 3× | Settings | how much busier than lately is fully busy (1.5× is ordinary) |
| `memory` | 3 s | Settings | how far back "lately" goes (video time) |
| `still` | 6 px/s | Advanced | motion slower than this counts as none |
| `foresight` | Parent | Advanced | the parent (on the source: the sketch being edited), the sketch being edited, or both |
| `react_to_hand` | off | Advanced | also slow down for the hand's speed and jiggle |
| `comfort` | 300 pt/s | Advanced | (with the hand) the on-screen speed the hand follows comfortably; *Calibrate* sets it |
| `jiggle` | 1.0 | Advanced | (with the hand) how strongly jiggle growth slows it |
| `slow_down` / `speed_up` | 0.1 s / 1.0 s | Advanced | response times, real time |

Measured (`tests/autospeed.rs`): editing a sketch ahead of a busy stretch (a 20 px/frame dash) plays at ×1.35 while the stretch is beyond the look-ahead and ×0.10 just before it; reading the parent through a child's view does the same; a Q press while it drives turns its ×2 into ×1.33 (÷ 1.5), and it keeps driving.

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

*(As built in M4.)* A **view** is a `frame` operator on anything with a box: a sketch, a tracker or a subject *(trackers and subjects added on request: "make it so i can focus on anything not just a sketch… generalize that feature")*. Per frame, a crop of the source `[cx, cy, crop_w, crop_h, canvas_w, canvas_h]` in source pixels. Its *canvas*, the view's own pixel grid, is as big as its widest crop ("display size = max size"): 1 view pixel = 1 source pixel at the widest framing, and the view magnifies as the crop shrinks. Views keep the video's aspect.

- **Every view maps straight to the source** (`SpaceMap`: `source = a · p + b`), so nesting doesn't need a transform chain. The chain is provenance: the breadcrumb, the parent's influence and zoom limits.
- **Strokes drawn in a view are stored through it.** The hand pipeline (lag, smoothing, jiggle) runs in the view's pixels, where the hand actually moved. Each stroke records the view's mapping for every frame it touched (`Through`), and its per-frame results reach the source through that record.
  - Consequence: re-tuning a parent view never moves a child sketch that already tracks something. *(Changed from "children re-derive when parents are re-tuned": a face track must not drift because the body's camera damping changed.)*
- **The motion union of a sketch drawn in a view is measured in that view** (its *home*, input `space`). In a stabilized view the subject barely moves, so nested sketches stay tight.
- **Entering views:** `Tab` (or the menu's *Enter view*, or a double-click on its timeline lane) enters the view of the selected sketch, tracker or subject; a selected stroke stands for its sketch, a look for its tracker. It creates the view on first use as one undo step, and clears the selection. So a hold inside a view starts a new sketch nested there. To edit what the view follows from inside it, click its box first.
  - A tracker's view nests in the view it tracks in (its guide sketch's); a subject's is a view of the source. Frames a tracker flags as lost are bridged, as in any box. A view goes when what it follows is deleted. A sketch drawn in a tracker's or subject's view is listed at the outliner's top level.
  - `Shift+Tab` backs out to the parent and selects what the view followed, so `Tab` goes straight back in.
  - A breadcrumb (`Source ▸ Sketch 1 ▸ …`, on its own layer so its clicks never reach the video) jumps to any level.
  - Each view keeps its own zoom and pan.
  - Outside the frames a sketch covers, its view holds the nearest framing, labelled in the breadcrumb.
  - A change of framing at the frame being looked at (an edit to the view's own sketch, a re-tune) eases in over 0.25 s instead of snapping.
- **Pan only (the default since hands-on use):** a view follows the subject and keeps it centred at its parent's scale (the source's, for a view of the source). It never zooms with the sketch; how close you look is the viewport's zoom, the wheel. Entering a view for the first time keeps the on-screen scale you had (screen points per video pixel), centred on the subject. The other modes, per view (`pan_only` off): a steady zoom, the widest the sketch needs (`lock_zoom`), or zooming with the region's size, smoothed. *(Changed after hands-on use: "when I'm viewing through a sketch my camera still goes wild with the box's size in zoom… just maintain what the user currently has".)*
- **The region always fits** (zooming modes): `fit` wins over the parent's influence and the zoom limits.
- **Zoom lock** (`lock_zoom`, on by default; the Settings tab sets it for new views): the crop keeps one size over the whole sketch, the widest the unlocked envelope (§10.1) reaches, `fit` included. It is still limited per frame by the parent's crop. A region that jitters in size then never makes the view zoom; panning is unchanged. Unlocked, the zoom follows the region through the envelope.
- **How a view follows, made visible** *(on request: "the very movement of the box itself can make something else seem shaky… if it was clearer in the settings how the viewport would account for such box movements, maybe even previews")*. A sketch's box wobbles a few pixels with the hand, and a view that follows it exactly makes a still picture shake inside it. Settings → Views explains this and sets it for new views (remembered): *Follow* closely / steadily / very steadily (the pan's smoothing 0.1 / 0.3 / 0.6 s and dead zone 0 / 8 / 18% of the view), the two sliders, and the zoom mode, with an animated **preview**: a hand-drawn box around a subject that stands, moves across and stands again (its few pixels of wobble included), through `frame_views` with those settings. On the left the source with the box (orange) and the view (blue); on the right what the view shows, a still scene as dots, which shake exactly as much as the view does. A button gives the project's existing views the same settings (one undo step); each view keeps all of them in the Inspector.
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

### 10.3 Layers: media attached to what's tracked

*(On request: "attach an image/gif/video to that tracking data … imagine im waving my hand at the camera and i want an epic face to be attached to my hand … thats basically how after effects does it"; decisions: "a layer can attach to anything with a position (and rotation optional)", "the size should be optionally following or scaling with any size data if it exists, or constant if not, also animatable and overridable manually through timeline", no text layers for now.)*

- **A layer is an operator** (`tt_core::layer`, kind `layer`): its input `target` is what it is attached to (a sketch, tracker or subject: anything with a position), its output where it sits on every frame the target has: `[x, y, angle, scale x, scale y, opacity, clip time, anchor x, anchor y, flags]`. So its lifetime, lane, undo, saving and re-evaluation when the target changes are the graph's, like a view's. A tracker's lost frames are bridged.
- **What it follows:** the target's point always; its angle when it has one (a subject) and *Turn with it* is on (the offset turns too); its box's size optionally (*Fixed*, *With the box*, its width or height), relative to the box on a reference frame (its first, or one chosen).
- **Its own values**, each fixed or keyed (`Animated`: linear between keys, held beyond): offset, scale (1 = its own pixels as video pixels; a new layer starts as tall as the target's box), rotation, opacity, anchor (the point of the picture on the target's point). Changing a value with keys keys it on the shown frame; one without keys changes its fixed value, unless **Auto-key** is on.
- **A clip's timing:** where in it it starts, its speed, and at its end: loop, hold the last frame, disappear, or back and forth (`LayerParams::clip_time`).
- **Media** (`tt_media::overlay`): pictures (PNG, JPEG, WebP, BMP, TIFF), GIFs and clips (MP4, MOV, WebM, MKV), read through ffmpeg as straight RGBA with their transparency, every frame evenly timed at the file's frame rate. The preview decodes each file once in the background, at most 512 px on its longer side and 192 MB for all its frames; an export reads it again at its own size.
- **In the app** (`tt_app::layers`): drawn on the video between the picture and the editor's marks, through the shown view, at its opacity; the selected one outlined (mint, `style::LAYER`) with its anchor. Right-click a sketch, tracker or subject: *Attach a picture, GIF or clip…*; or drop a picture or GIF on the window with one selected. The Inspector has its file (*Locate…* when it moved), *Attached to*, the follow options, a clip's timing and every value with its key button and previous/next key arrows. The timeline's lane shows its frames and keys; the outliner lists it under what it's attached to.
- **Clicks and drags:** a click on a layer's picture selects it (a second click reaches what's under it). In the Select tool (`layer::Handle`): a drag on its picture moves its offset; on the selected layer, a drag on a corner (a square, 8 pt) scales it about its anchor, one just outside a corner (to 2.5 × that) turns it about its anchor, and Alt+drag on its picture moves its anchor while the picture stays put (the offset makes up for it). Each is one undo step, and keys on the shown frame by the same rule as the Inspector; the pointer says which (resize, turn, move, crosshair). The selected layer or subject goes first when they overlap.
- **Smoothing** (seconds, zero-phase, per layer): what it follows is smoothed for the layer alone (point, box size, angle), so the tracking's jitter goes without touching the tracking.
- **Stacking:** layers stack by `depth`, then the order they were made; *Bring to front* / *Send to back* (menu) and Back/Down/Up/Front (Inspector) renumber them, one undo step.
- **Blend modes** (exports; the preview shows Normal and says so): Normal, Add, Screen, Multiply, mixed in R'G'B' with the pixel under the layer (converted back from the source's codes); alone on transparency every layer is Normal.
- **Exports** *(on request: "the option to export such layer without the video, e.g. transparent. also a raw json option for raw motion data instead of rendering a moving square (but keep that)")* (`tt_media::layers`, `tt_app::panels::layer_export`; right-click a layer: *Export layers…*):
  - **Over the video:** read as 4:4:4 (16 bits for deeper sources), each layer blended straight into those planes in the source's own matrix and range, so pixels no layer covers come out exactly as they were; ProRes, DNxHR or H.264 with the sound and colour tags, as the stabilized export.
  - **Alone, on transparency:** the video's size, rate and frames, to line up on a track above it: ProRes 4444 with alpha (.mov), a PNG sequence (a folder, numbered by frame) or WebM (VP9 with alpha).
  - The layers' media is read again at its own size for it (1.5 GB for all of them at most), on the export's thread; bilinear, premultiplied while sampling (no dark fringes), each layer's alpha times its opacity, bottom first.
  - **Motion data (JSON)** (*Save its motion as data*, on any layer or anything tracked, or the window's third choice): per frame, each layer's anchor point, angle, scale, opacity, clip time and corners; each tracked thing's point, box, angle (a subject's) and whether the frame can be trusted; source px (y down) and fractions of the frame.
  - **After Effects keyframes** (*Copy as After Effects keyframes*): Transform position (and rotation, scale and opacity for a layer; rotation for a subject), a key a frame from the in point, which is the comp's frame 0.
  - The tracking target video stays (§ the stabilizer's export window).
  - **In the stabilized video** *(on request: "if i export tracked video can i see the attached images/gif/videos in the preview and have it included in the export … we kinda needed that to apply to the exported videos as well")*: the stabilizer's export window has *Include the layers* (on by default, shown when there are layers). Each layer is blended into the source frame as above, then the frame is stabilized (`render_warped_with`), so the layers move with what they are attached to, as on the video. Its preview draws them with their preview pictures through the export's map (blend modes as Normal, as the viewport). An export asked for while layers are still being worked out starts when they are. Measured (`tt_media/tests/layers.rs`): a square the map follows stays at the same output pixel on every frame, with the grey beside it untouched.
- Measured (`tests/layer.rs`, `layer::tests`, `overlay::tests`): a layer rides on a subject's point and moves with it; a drag moves the fixed offset everywhere, and with Auto-key keys it on the shown frame; one undo each; it saves and loads with its keys and modes; clips loop, hold, hide and ping-pong at the right times; a GIF, a PNG with alpha and an MP4 are probed and decoded; (`tests/layers.rs` in tt_media) a red square layer over a grey clip renders red where it is and leaves the grey untouched beside it, and alone it comes out opaque on transparency in ProRes 4444, PNGs and WebM; the JSON and keyframe text read back the right values from the in point.

### 10.4 SpringFocus: moving from one tracked thing to the next

*(On request: "if at the start of the clip i already zoomed in and centered onto something but then i later want to do a tracked mouse shot … without having to hard cut to the mouse and still being able to move the camera smoothly over to it"; "make something called a SpringFocus, make some interface for me to set whatever its tracking at any time, spring settings in inspector".)*

- **An operator** (`tt_core::focus`, kind `focus`) with keys: from a frame on, focus on a target (a sketch, tracker or subject; its inputs `target` follow its keys). Its output is a box in a tracker's layout, so it is followable: Tab follows it (the smooth camera), a layer attaches to it, a subject can take it as a member, and the stabilized export renders it (it counts as a point there).
- **No lag:** at a key it doesn't chase the new target with a spring (that trails a moving mouse); it blends from what it showed without this key to the new target's own motion by a spring's step response, so both ends keep moving during the move and it lands exactly on the new target at the move time, then sits on it. A key during a move starts from wherever that move was. The box's size moves over the same way.
- **The spring** (Inspector): move time (to land; 0 is a cut), bounce (damping ratio 1 − bounce: past and back), lead (it starts this long before the key). The step response is tuned to be within 0.2 % at the move time and the remainder added smoothly over the move, so it lands exactly, with no snap. A graph shows one move.
- **Setting what it focuses on:** right-click a sketch, tracker or subject: *Make a SpringFocus on it*, or *SpringFocus N: move over to it here* (a key on the shown frame); in the Inspector, *Move over to…* on the shown frame, and its keys listed (frame, target, Go to, Remove). On the video: a viewfinder's corners round its box, a dashed line to what it's moving to, its name and target; the timeline: its frames, its keys (diamonds in each target's colour) and its moves shaded. A click on its point selects it.
- Measured (`tests/focus.rs`, `focus::tests`): on the still target before the key, between them during the move, exactly on the moving target from the move time on (no lag), every step forward and under 60 px; a slower spring is slower and undoes in one step; a view entered with Tab sits on the new target after the move; keys and targets save and load; a bounce overshoots; a key mid-move starts from where it was; a lead starts it sooner.

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
- **Creation order:** every document entity carries `Created(u64)`, stamped when it gets its first document component and saved. Lists that show things in the order they were made (the outliner, the timeline's lanes, Select All) sort by it, never by entity id: bevy reuses freed ids.
- **Project file:** a single SQLite database (`*.ttproj`).
  - Entities carry `StableId(Uuid)`; entity references are mapped through it.
  - Components are stored as versioned RON via reflection.
  - Signals and streams are stored as content-hashed, zstd-compressed chunk blobs; only those a saved component refers to (a deleted entity's signals stay in memory for undo, not in the file).
  - Saves are incremental (only dirty chunks), transactional and crash-safe.
  - Schema migrations are versioned from day one.
- **Several projects on one video** *(on request: "add save file option so we can save specific project info so we can have multiple projects on the same video file")*: each video has its own project in the data folder (`projects/<key>.ttproj`) and can have more anywhere (`tt_app::project`). The top bar's *Project* menu, beside the video's name, lists the video's projects (the open one selected; a file that has gone can be taken off the list). It also has *New project…* (an empty project in a file you choose; the open one is saved first), *Save project as…* (a copy; changes then save to the new file and the old file keeps what it had), *Open project…* (a `.ttproj` with its video: the `media.path` it was saved with; on the open video when that path is gone) and *Show the project file*. The session remembers each video's projects, the one used last first, so a video opens with the project used last on it, after a restart too. Autosave writes to whichever project is open.
- **Media paths** are stored both absolute and relative to the project, fixing v1's orphaned-project problem.

### 12.1 Starting again after an error

Two errors end the app: the graphics card's driver resetting the card (the app's wgpu device is then lost; eframe can't make a new one, and egui-wgpu panics when it paints with it), and a panic on the main thread. Instead of a message box and a dead window, trackertools starts again where it was (`tt_app::recover`):
- **The loss is found between frames** (a submit or a present finds it, or the non-blocking poll that starts every `Shell::logic`: a reset while the app waited shows there): wgpu's device-lost callback notes it, and `Shell::logic` (which runs before anything is painted) saves the project and the session at the shown frame, starts a new trackertools and exits. Nothing is lost.
- **The loss is found while painting** (egui-wgpu's buffers for the frame: where a reset usually shows), or **a panic**: the callback and the panic come in the same call, with no `logic` between. The panic hook starts a new trackertools and exits. It can't save (the world may be half changed), so the last autosave counts: edits from the last seconds can be missing, and the frame is the one last remembered (while paused). A running tracker's results count as a change every 5 s, so autosave keeps them up to then too.
- The new process waits for the old one to exit (its process id), reopens the last video at its frame with its project, and says what happened in a small window (with *Report a problem*), honestly about what can be missing. CoTracker trackers are paused, as on every open (§6.2), which breaks the loop that a crash under CoTracker made.
- Not twice in 5 minutes, and not in a run's first 10 s (an error at every start would loop): then the message box of before. `logs/recovery.json` keeps the last one (and when trackertools last started itself again, through refusals), and the report includes it.
- A wgpu validation error is logged instead of panicking (the first few, then one in a thousand) and the app goes on without what failed; running out of graphics memory, or an internal error, counts as a loss. A video larger than the card's textures isn't shown, instead of failing every frame.
- A panic in a tracker's job thread is that tracker's error (the job catches it): the app goes on. A panic in any other thread is logged as one, and the next start offers the report.
- Release builds only (`TT_RECOVER_TEST=1` in a dev build); never in scripted runs; `TT_NO_RECOVER` turns it off. `TT_SIMULATE_GPU_LOSS=<seconds>[,panic]` destroys the app's own device (the same lost device a reset makes, without touching the card) to test it: plain, through the saving path; `,panic`, through egui-wgpu's panic.

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
8. **Releases and updates.** The repository is the source of truth: pushing a tag `vX.Y.Z` runs `.github/workflows/release.yml`, which builds the app with `TT_VERSION` set to the tag and publishes `trackertools-windows-x64.zip` (the program alone: the app sets FFmpeg up itself, see 9). `scripts/package_windows.ps1` makes the same program locally as one file to send someone (`dist/trackertools.exe`, the C runtime linked in, no console window); `-InstallTo <folder>` also puts it in a folder on the PATH (the user's own copy: `C:pps`, refreshed by a `buildtrackertools.cmd` there). A copy that is running is renamed aside, and the new one starts next time. Settings → Updates (`tt_app::update`) reads the latest release from GitHub's API, and installs it in one click: `curl` downloads it and `tar` unpacks it (both part of Windows 10/11 and macOS, so no HTTP or zip crates), each replaced file is renamed to `<name>.old` (Windows renames a running program but won't overwrite it) and the new one moved in, then the app saves, closes and starts the new version; the `.old` files go at the next start. A quiet check runs at start (a setting), and a "⬆ Update to …" button appears in the top bar when there's a new version. A copy built from source says "-dev" and leaves updating to git. Others only see releases if the repository is public.
9. **Setup and the doctor** (`tt_app::setup`). FFmpeg isn't bundled *(changed on request: "we shouldn't need to bundle in ffmpeg and ffprobe")*. At start, `tt_media::ffmpeg::tool` looks in the FFmpeg folder chosen at setup (`ffmpeg-location.txt` in the data folder; by default `ffmpeg\` there) after `FFMPEG`/`FFPROBE`, then beside the program and on the PATH. If ffmpeg and ffprobe don't both start, a small window comes before the app: choose a folder, *Install FFmpeg* (gyan.dev's latest "essentials" release for Windows, about 115 MB, checked against its published SHA-256, unpacked with Windows' own `tar`; only `ffmpeg.exe`, `ffprobe.exe` and FFmpeg's license are kept), or *Find FFmpeg* (a folder that has them, or its `bin`). Then *Start trackertools* turns the window into the app. The checks: both programs and their versions, the encoders exports need and the decoders for H.264 and HEVC, the graphics adapter (a software renderer warns), the data folder being writable, curl and tar for updates, CoTracker. *Copy report* / *Save report* give a text to send: the checks, this computer, and the ends of this run's and the last run's logs. The app now logs to `logs	rackertools.log` in the data folder (the last run's kept beside it), panics included; after a run that panicked, the top bar shows *Report a problem*. If the window can't start at all (graphics), the report goes to the data folder and a message box says where. The doctor also sets up CoTracker (§6.3). Every instruction it shows is written in ASD-STE100 Simplified Technical English (short, present tense, one instruction a sentence, no contractions) *(on request: "they're not gonna read a readme")*: the README went, the program is the one file.

---

## 14. UI

- **Shell:** eframe (wgpu backend) + `egui_tiles` docking. The layout is session state in the world.
- **The visual language** *(on request: "the override and automatic data should be visually distinguishable, probably good to establish a consistent visual language for the entire app")* (`tt_app::style`, `tt_app::icons`): a hue says where data came from, the same in the viewport, the timeline, the outliner and the inspector.
  - **cyan**: computed by an algorithm (a tracker's automatic results); the interface's own accent is the same cyan (buttons, the playhead).
  - **orange**: drawn by a person: sketches, a tracker's drawn frames, manual dots.
  - **Each sketch has its own colour** *(on request: "add color changing feature to sketch so it can be more visible depending on the background"; "trackers adopted the colors of the sketches they are contained in")* (`tt_app::colors`): orange for the first, then a palette that keeps clear of the colours above, in the order sketches were made, or one picked in the Inspector (swatches, `Tint`, one undo step; Auto goes back to the palette). Its box, path, lane and icon use it, and a tracker following a sketch shows what it found in the sketch's colour (cyan with no sketch). Lost frames stay red, drawn frames orange, a stroke being recorded amber.
  - **Boxes show on any picture** *(on request: "a slight border with the sketch boundary/box so that its a negative of below it? … or just a black/white animated outline like marching ants")*: every box and cross has a thin dark edge; the selected sketch's box and the one being recorded get marching ants (black and white, moving at 15 frames a second, only while one is on screen; Settings turns it off). A true negative needs the video's shader and was left for later.
  - **Where the editor is, not the video** *(on request: "if you are in a sketch view, the outside of the box should be like dimmed … with an animated dither pattern so we KNOW its the editor and not the video itself")*: outside the picture, a grey checkerboard (the video shader), so a black video and no video differ. Inside a view, outside the box it follows is darker with slowly moving diagonal lines (Settings turns it off), the box it follows is dashed, and the corner says "Inside Sketch 1's view · Shift+Tab leaves". While a stroke records, a red frame goes round the video; with the Sketch tool on and nothing selected, a line at the bottom says how to start.
  - **white**: what a person told an algorithm: a template tracker's looks (rectangles), a CoTracker's reset points (diamonds).
  - **purple** subjects, **blue** views, **red** lost or untrusted frames and errors, **amber** being recorded now (and warnings).
  - Solid and bright: valid and selected; dim: stale or not selected; dashed and light: a preview of what a press would make.
  - **Icons are drawn as shapes, never font symbols** (`icons::Glyph`): a sketch is a box with its point, a view a viewfinder's corners, a stroke a wiggle, a template tracker a box with a crosshair, a CoTracker a ring with one, a manual dot a solid dot, a look a white rectangle, a reset point a white diamond, a subject a diamond with its heading. *(On request: "some symbol boxes missing with a square glyph".)* Text keeps to characters egui's fonts have: a test reads every string literal in the crates and checks each character outside ASCII against the fonts' own character maps (`icons::tests`; egui's `has_glyph` says no to characters in its replacement glyph's font, NotoEmoji's ✔ and ▶ among them, which draw fine). The arrows that showed as boxes (key names like "Shift+←", the tracker's progress) are words now ("Shift+Left").
  - **What a tracker is doing**: a spinner (`icons::Activity`) beside it on the video, at the start of its lane and in the Inspector: a turning arc while it starts or tracks (amber while CoTracker loads its model), a slow pulse while it waits for the playhead, two bars when paused, a dot when done, red when it failed.
- **Panels** (each a function of the world that emits intents):
  - **Viewport** (N instances): shows a View; overlays come from registered overlay providers.
  - **Timeline:** a ruler, transport, lanes per entity from registered lane providers, summaries (§5), and key editing.
  - **Inspector:** reflection-driven, plus custom widgets.
  - **Outliner:** media → views tree → captures / trackers / targets.
  - **Jobs/status.**
  - Later: **Curve editor**, **Operator graph** (egui-snarl).
- **Tools** (Select, Sketch, Adjust, Pan/Zoom) are state machines in the `Tools` set, fed by a `PointerFrame` resource (every timestamped sample since the last frame, in source pixels) and `KeysHeld`, so they run headless in tests. An in-progress gesture is world state (the Sketch tool's `LiveCapture`), so overlays draw it; on commit it becomes document entities in one transaction.
- **Selecting and commands** work the same in the Outliner, the Timeline and the Viewport.
  - Click selects, Ctrl+click toggles, Shift+click adds (a range in the Outliner).
  - Dragging a box over Outliner rows or Timeline lanes selects what it touches.
  - Right-click opens one shared menu: enter view, rename, duplicate, delete, select its strokes or its sketch, select all.
  - Each command is one undo step (`tt_core::commands`).
  - A box keeps its start on the content: dragged past the edge it scrolls the list, and what it swept stays in it. Esc drops it.
  - The Outliner is a tree: sketches, the sketches drawn in their views, and their strokes (folded). It has a filter box, middle-drag scrolling, and unfolds and scrolls to what is selected elsewhere (only when it is out of view).
  - The Timeline scrubs from the ruler (a click seeks). With snapping on (`N`, or the header's "snap"; Ctrl while scrubbing inverts it), the playhead snaps within 8 points to the first and last frame of every sketch, stroke, view and tracker (their lifetimes' ends, §5.1), marked on the ruler. The right-click menu can send the playhead to the selected thing's start or end. Its lanes follow the tree, scroll vertically (wheel, middle-drag, scrollbar), show a starting stroke's lane, and a double-click enters a sketch's view.
  - **Lanes:** subjects first, then each sketch, its view and its trackers (indented), then the sketches nested in its view; trackers with no sketch and manual dots last. A column on the left names each lane: its icon and (a tracker) its spinner, then the name, clipped; time runs to the right of it, and the ruler's corner above it says the playhead's frame. Rows alternate in shade, the one under the pointer lights up, the playhead has a head on the ruler with its frame number. A tracker's lane shows its two layers: frames drawn by hand along the top (orange; a manual dot's are its whole bar) over its automatic results (cyan), its looks (white squares) or reset points (white diamonds) on their frames, its score as a line along the bottom (per pixel column, the lowest score there, so a one-frame dip shows on a long clip; a frame wider than a pixel fills every column it covers, so the line doesn't break), its flagged frames in red, and each running job: an outline over what it still has to track and a mark where it is (grey while it waits for the playhead, amber while CoTracker loads). A subject's lane shows its offset keys as purple diamonds. *(On request: "the timeline could use some nice visual UX as well, an upgrade there".)*
  - **One tree** *(on request: "is it possible for sketches to be made underneath a tracker or anything with a position … how to generalize and unify the logic of this?")* (`tt_app::tree`): one relation places everything under what it was made relative to (`parent_of`): a sketch drawn inside a view under what that view follows (a sketch, tracker or subject), a tracker under its guide sketch or what the view it tracks in follows, a layer under what it is attached to; subjects are roots. The outliner and the timeline's lanes both walk it (subjects first, then in the order things were made, as deep as it goes); a sketch's strokes, a tracker's looks, a subject's members (as references) and views hang off their owner. A parent cycle (a damaged file) is listed at the top.
  - **The Outliner** draws each row's icon (the visual language's), folds with drawn triangles, and lists trackers with no sketch and manual dots at the top level, each with its looks or reset points.
  - **In and out points** (§3): the header has *In*, *Out*, the range (`20–44 · 25 fr`) and *Clear*. On the ruler, a bar along its foot from in to out and a bracket on each point marked; outside the range the ruler and lanes are shaded. Drag a bracket to move its point (one undo step; it snaps like the playhead, and the playhead snaps to it): the playhead goes with it, so the viewport shows the frame the export starts or ends on. The viewport marks those frames with a bracket down the picture's left edge (in) or right edge (out) and a label; outside the range it notes "outside in/out", and while the export window is open (or a bracket is being dragged) it dims the picture there.
  - **Lifetimes:** drag either end of a lane to say when that object begins and ends (§5.1); the pointer turns into a resize arrow within 5 points of an end. With snapping on, an end snaps to the playhead and to the other objects' ends. One undo step per drag; frames outside the lifetime stay, drawn faint.
- **Settings tab** (beside the Inspector), remembered in the session file (scripted runs, the demo and benchmarks, neither use nor save it):
  - the **Brush** tab: the next stroke's size and falloff, what the wheel does, anticipatory speed on/off, and the box of the selected sketch (or of new sketches): padding and smallest box in px of the space drawn on (video pixels on the source, view pixels inside a view, with the conversion shown), jiggle gain, hand lag, presets. Before a stroke, a dashed outline at the cursor shows the box a still hand would get;
  - the size and falloff the next stroke starts with;
  - the preset new sketches use;
  - while holding a stroke: hide the pointer, and the clear window's radius (§8.1);
  - anticipatory speed: on/off and its knobs (§8.4);
  - whether new views keep a steady zoom (§10);
  - every key, generated from the keymap;
  - where the data folder is.
- **Keymap** is data (a resource), rebindable, with a help overlay generated from it. Defaults are **Blender-like**:
  - `Space` play (also while holding the button: recording across frames);
  - `D` Sketch tool; click selects; `Shift`+hold starts a new sketch; `Ctrl`+hold moves only; arrow keys while holding retake frame by frame; the wheel holds still while holding (or, as a setting, zooms or sets the stroke's size, falloff or both); `Esc` cancels the stroke or leaves the tool;
  - `Alt+A` deselects, `A` selects all sketches;
  - `X` / `Delete` deletes the selection (a sketch with its strokes and view; a stroke leaves its sketch), `Shift+D` duplicates sketches with their strokes (both wait for a stroke to end), `F2` renames;
  - `J` / `K` / `L` shuttle: backward, play/pause, forward; J or L again doubles the speed, up to 8× (§3);
  - `Q` / `E` slower / faster playback (it is also the capture speed; `[` / `]` work too). The speed is always shown in a badge top-right in the viewport, amber when not 1×, and flashes large in the middle when you change it (fully for 0.25 s, then a 0.3 s fade; auto speed's changes don't flash, §8.4);
  - `←/→` step, `Shift+←/→` jump to start/end;
  - `I` / `O` mark the in / out point at the playhead, `Alt+X` clears them, `Shift+I` / `Shift+O` go to them (as in Resolve);
  - `T` the Track tool, `M` the Draw tool (a tracker's point by hand, or a manual dot; §6.4); a click selects the tracker whose point is under it too;
  - `H` switches the selected trackers off from the playhead on (or on again there), `Shift+H` off everywhere (or on everywhere) (§6.5);
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
