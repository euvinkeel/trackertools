# trackertools v2: roadmap

Status: draft for review, 2026-09-27. Companion: [DESIGN.md](DESIGN.md).

**First target:** a video loads and plays back smoothly (M1), on a core that can grow (M2); then the flagship features, which are motion sketching and smoothing (M3) and derived views (M4). Everything after M4 is sketched only, and will be re-planned once M4 is in hand.

**Working agreement:**
- Each milestone ends with a demo you can run, plus automated tests.
- Nothing is committed unless you ask.
- Spikes are time-boxed; if one fails, the plan changes before code piles up on it.

---

## M0 · Skeleton ✅ done 2026-09-27

**As built:**
- workspace `crates/{tt_core, tt_media, tt_app}` + `xtask`;
- `cargo run -p tt_app` opens the docked layout, driven by a one-minute demo clock;
- `cargo test --workspace`: 14 tests; `cargo clippy -D warnings` clean;
- `cargo xtask fixtures`: 6 clips + manifest + sprite truth, byte-deterministic, and each counter clip carries a binary frame-index barcode (verified: decoded pixels read back the true index, including across VFR gaps).

**Learned:**
- bevy 0.19 stores resources as entities (`IsResource`), and `ReflectResource` is only a marker. Reflection of resources goes through the component path. This matters for the M2 inspector and save format.
- eframe 0.36's `App` is `logic()` (before every pass, even hidden) + `ui()`.
- egui 0.36 renamed `show_inside` to `show`.

The original plan follows.

**Goal:** the workspace exists, and a window shows an empty docked layout driven by the world.

- A Cargo workspace (`tt_core`, `tt_media`, `tt_app`) with pinned versions (DESIGN §16) and `tracing` logging.
- `tt_core`:
  - a `World` + two schedules (pre-UI, post-UI) with the system sets from DESIGN §2;
  - the `Module` / `AppBuilder` registration API (components with meta, systems; operators arrive in M2);
  - `Transport` and `Playhead` resources.
- `tt_app`:
  - an eframe window on the wgpu backend, with an `egui_tiles` layout of Viewport, Timeline and Inspector placeholders;
  - the layout stored as session state in the world;
  - a dev panel listing entities and resources, a precursor to the outliner and inspector.
- `tools/make_fixtures`: generates test clips with ffmpeg lavfi, **in the repo**, fixing v1's out-of-repo fixtures:
  - 1080p60 `testsrc2` with a burned-in frame number, keyframe interval 250 and 1;
  - a variable-frame-rate clip;
  - an HEVC clip;
  - a moving-sprite clip with known ground truth.

**Accept:** `cargo run` opens the layout; `cargo test` passes; the fixtures regenerate deterministically.

---

## M1 · Media & smooth playback: mostly done 2026-09-27; S3 awaits a hands-on run

**Status (as built, measured on the P5 recording):**

| Item | Status |
|---|---|
| Import | ✅ dialog (Ctrl+O), drag-and-drop, command line, Recent menu; indexing takes 65 ms. ⏳ MKV remux on import |
| Decode service | ✅ read-ahead scaled by rate; paused backward fill one decode group at a time; stream reuse instead of respawn |
| Frame cache | ✅ 2 GB NV12, evicts the frames farthest from the playhead; the nearest frame stands in, never a blank |
| Viewport | ✅ NV12 → RGB shader (BT.709/601, limited/full), wheel zoom about the cursor, drag pan, nearest sampling at ≥3×. ⏳ pixel grid |
| Transport and timeline | ✅ rates 0.1–4×, steps, zoomable/pannable timeline that follows the playhead, decode-cache strip. J/K/L shuttle (DaVinci-style: backward/forward from 1×, doubling per press up to 8×; K play/pause), with backward playback decoded a keyframe group at a time |
| Proxy | ✅ auto for GOP > 30. NVENC builds P5 in 60 s (1,153 fps); alignment verified 1:1 on every fixture + content check on P5. The viewport shows the proxy unless it would be magnified |
| Session | ✅ `%LOCALAPPDATA%\trackertools\session.json` restores the last file and frame (the `.ttproj` project file arrives with M2) |

**Acceptance so far:**
- 75 s at 1× while the proxy builds in parallel: 4,501 frames at 60.0 fps, 0 UI frames without the exact frame.
- With the proxy:
  - step forward 5.6 ms;
  - step back median 5.7 ms / p95 23 ms / max 98 ms (the first step right after a jump);
  - seek median 92 ms.
- **S3 ✅** ([spikes/S3-input.md](spikes/S3-input.md)):
  - Windows raw input on a dedicated thread gives ~1 kHz reports (p50 spacing 1.00 ms), monotonic QPC timestamps, and the absolute cursor on ~95% of reports;
  - egui gives about one untimestamped event per UI frame;
  - so sketch capture uses the raw-input service.
- **5-minute run ✅:** 300 s of P5, 17,983 distinct frames (59.9 fps), 0 UI frames without the exact frame, one decoder spawn. 17 frames (0.09%) went undisplayed because of UI hitches, never because the decoder was late.
- **4K ✅ (stretch):** 15 s of 3840×2160, 0 UI frames without the exact frame, 59.1 fps shown (a few frames skipped around the switch to the proxy).
- ⏳ Left for later: MKV remux on import, pixel grid at high zoom.

The original plan follows.

**Goal:** open any of your recordings and scrub, step and play them with frame exactness.

**Spikes first** (each ≤ 1 day; results recorded in `docs/v2/spikes/`):

| Spike | Question | Pass criterion |
|---|---|---|
| **S1** decode path | Does Rerun's approach (re_mp4 index → Annex-B → `ffmpeg-sidecar` → NV12) decode the 69,470-frame P5 file frame-exactly, including B-frame reorder and any open keyframe groups? | 50 random frames are byte-identical to an `ffmpeg -vf select` reference |
| **S2** display | NV12 upload + shader through an egui_wgpu `CallbackTrait` at 1080p60 and 4K60 | ≤ 2 ms GPU per frame; no frame hitches in a 60 s trace |
| **S3** input | Can we get every pointer event with a usable timestamp through eframe (winit hook)? | ≥ 500 Hz with a gaming mouse, timestamps monotonic, jitter ≤ 1 ms |

**S1 ✅ passed** (see [spikes/S1-decode.md](spikes/S1-decode.md)):
- exact on the P5 file and on every fixture, after an open-GOP fix;
- 657 fps sequential; ~190 ms per random seek, so the frame cache + background keyframe-group fill + proxy below are required, not optional.

**S2 ✅ passed** on the P5 file, 1080p60 at 1× (`TT_AUTOPLAY_SECS=10`):
- 601 distinct frames shown in 10.00 s (60.1 fps), 0 of 1,749 UI frames lacking the exact frame;
- texture upload 0.64 ms CPU, UI 5 ms per frame at 175 Hz, one decoder spawn.
- 4K60 is not yet measured.

**Steps and seeks on P5, no proxy** (`TT_BENCH_STEPS=1`, time until the exact frame is on screen):
- step forward: median 5.7 ms;
- step back: median 5.8 ms, p95 177 ms, max 401 ms. The slow ones come right after a jump, while the previous 250-frame group decodes;
- seek: median 218 ms.

The proxy (item 6) exists for the step-back tail and for seeks.

**Build:**
1. **Import:**
   - an open dialog (`rfd`), drag-and-drop and a recent list;
   - index the file (re_mp4), and remux non-MP4 containers to MP4 with `-c copy`;
   - create a `Media` entity with a frame grid (rational fps, VFR rule) and a keyframe index.
2. **Decoders:** a playback decoder (sequential read-ahead) and a seek decoder (keyframe-aware, latest-request-wins), running on worker threads and reporting through channels.
3. **Frame cache:** an NV12 LRU (2 GB default) with a read-ahead / read-behind window, and nearest-cached-frame-first display during scrubs.
4. **Viewport:** NV12 → RGB shader, zoom / pan / fit, and a pixel grid at high zoom.
5. **Transport:**
   - play/pause, rates (1×, ½×, ¼×, 1/10×), step ±1 and ±10, home/end;
   - a frame field and timecode;
   - a timeline ruler with scrubbing;
   - frame pacing from the wall clock (the displayed frame is chosen per vsync, never "one decode per frame").
6. **Proxy:**
   - a background NVENC transcode (keyframe every 12 frames, no B-frames), with progress in a Jobs panel;
   - used automatically for scrubbing and backward stepping;
   - the original is used when zoomed past the proxy's resolution.
7. **Persistence (minimal):** a `.ttproj` SQLite file holding the media reference (absolute + relative path) and session state (playhead, layout).

**Accept** (measured on your P5 file and the fixtures):
- Index opens in ≤ 2 s; 1× playback has 0 dropped frames over 5 minutes.
- Step forward ≤ 16 ms (cached). Step back ≤ 50 ms with the proxy, ≤ 250 ms without.
- Burned-in frame numbers always match the displayed frame index: stepping, scrubbing, after seeks, and at clip ends.
- Relaunch restores the last file and playhead.

---

## M2 · The core that everything plugs into: in progress

**Done 2026-09-27 (`tt_core`, 36 workspace tests, clippy clean):**
- **SignalStore:**
  - 256-frame copy-on-write chunks;
  - absent / valid / stale per frame;
  - runs, and chunk diffs for undo.
- **RangeSet:** dirty-range arithmetic.
- **Operator graph:**
  - kinds + reflected parameter components;
  - `Inputs` / `Output` edges; topological order;
  - cycle errors;
  - footprint-exact invalidation (tested: an edit at frame 50 marks exactly 50 / 48–52 / 48..end through pointwise / window / causal operators);
  - budgeted upstream-first evaluation;
  - disabled producers count as disconnected.
- **Transactions:**
  - `edit(world, label, |tx| …)` with typed undo/redo;
  - delete = `Disabled`, so ids stay stable;
  - signal snapshots with invalidation on undo;
  - gestures as one step;
  - a randomized 200-edit undo-all / redo-all test.
- **Keys and UI:** Ctrl+Z / Ctrl+Shift+Z (or Ctrl+Y), plus top-bar buttons with labels.

**Also done:**
- **`.ttproj` persistence** (SQLite):
  - generic reflection (RON) for every document component;
  - entity references remapped on load by a reflection walk;
  - content-addressed LZ4 signal chunks (a one-frame edit writes 1 chunk; identical chunks dedupe);
  - deleted entities skipped, and so are signals no saved component refers to (found by a reflection walk; older files drop them on load);
  - a saved creation order (`Created`), so lists keep their order although bevy reuses freed entity ids;
  - format version guard.

  Tested in `tests/persist.rs`.
- **Per-video project autosave:** in `%LOCALAPPDATA%\trackertools\projects`, keyed like the proxy. Loads on open, saves 1.5 s after edits settle, before switching videos, and on exit.
- **Reflected edits (`Tx::set_reflected`):** any registered component is editable and undoable by type path. Tested.
- **Selection** (session).
- **Outliner:** named document entities, click / Ctrl+click.
- **Generic Inspector:** editable widgets for numbers, booleans, text and enums, with nested structs. Drags are one undo step; session components apply without undo.

**Moved to M3:** timeline summary pyramids (they belong with the lanes).

The original plan follows.

**Goal:** the state machinery from DESIGN §5–§7 and §11–§12, fully tested headless before any flagship feature depends on it.

1. **SignalStore:**
   - signals and streams, 256-frame copy-on-write chunks;
   - validity / stale / absent per frame, versions, provenance;
   - summary pyramids.
2. **Operators:**
   - registry and vtables;
   - **typed ports** (DESIGN §6.1) with implicit `Lift` adapters between spaces;
   - `Inputs` + `Dependents` index (hooks);
   - footprint classes;
   - dirty-range propagation;
   - demand-driven, budgeted evaluation;
   - topological order and cycle rejection.
3. **Transactions:**
   - `world.edit`, with reflect-based before/after for components;
   - spawn/despawn bundles;
   - signal chunk copy-on-write references;
   - gesture-scoped transactions;
   - selection snapshots;
   - the debug audit for untracked document mutations.
4. **Persistence:**
   - `StableId` mapping;
   - RON components;
   - content-hashed zstd chunk blobs;
   - incremental, crash-safe saves;
   - schema version + migration hook.
5. **Inspector:** reflection-driven editing of any Document component, with edits going through transactions (so they're undoable automatically).
6. **Outliner:** media → views tree → everything else.

**Accept:**
- Property tests: random edit sequences → undo all → the world is identical to the start; redo all → identical to the end.
- Save → load round-trips byte-identically.
- Invalidation tests for every footprint class.
- A 70k-frame signal edit re-derives only the footprint range.
- Evaluation never exceeds its frame budget.

---

## M3 · Motion sketch & smoothing (flagship): in progress

**Status (as built, 2026-09-27):**

| Item | Status |
|---|---|
| Sketch tool | ✅ `D` arms it. **Auto-key model** (reworked after hands-on use): holding the button records against whatever frame is shown and never touches the transport. Paused, it edits that instant; tap Space while holding to record across frames. Each press → release is a *stroke* on the selected sketch, or a new sketch with nothing selected or with Shift. Each stroke is **one undo step**. `＋ New sketch` (or Shift+hold, or clicking empty video) starts a new sketch. A quick click selects the sketch under it and never records. Ctrl+hold moves only (keeps the box size). Esc cancels, Alt+A deselects. If the edited sketch is undone mid-stroke, the stroke starts a new sketch instead of writing into the deleted one |
| Wheel and settings | ✅ While holding, the wheel does nothing by default: the view holds still under the hand (`WheelMode::Still`, settings version 4; an old default of Zoom takes it, a chosen mode stays). Otherwise the wheel zooms the view. The stroke's size, falloff and lag are set beforehand in a **Brush** tab, the "preparatory side panel". That tab also shows the box settings of the selected sketch (or of new sketches) in the pixels you draw on, what a still and a jiggling hand get, and presets. A dashed outline at the cursor shows the brush's still-hand box. As a setting, the wheel can still set the size, the falloff or both: one knob position, so turning back returns both exactly; one notch up from a falloff of 0 moves it off 0; on a move-only (Ctrl) stroke it sets the falloff. A size below 1 keeps `min_half` as the floor. Remembered between launches (settings version 2: older files take the new wheel and falloff); a bad value resets the settings, not the recent files. The demo and the step benchmark neither use nor save them |
| Strokes and retakes | ✅ `layer_over`: a stroke replaces the frames it visited (× influence) and nothing else. The region around them re-derives from the new data, so a hold says where the mouse *should* have been and the box jumps and grows to take it in. A held frame takes where the hand sat on it (median of its last 0.15 s), unsmoothed. Stepping (arrow keys) or playing while holding retakes each frame shown (tests/retake.rs; demo: three frames retaken 30 px right land at +30.2 / +30.2 / +29.9, neighbours unchanged). An optional falloff (0 by default; it used to be 0.2 s and dragged neighbours) still pulls frames beside a stroke along (Blender's smooth falloff, normalised where edits overlap). Short gaps in new territory are bridged. Stroke falloff, influence and lag are editable afterwards (the lag is per stroke, in real seconds, taken from the sketch's `lag` when drawn) |
| Pointer input | ✅ a raw-input service thread (1 kHz, QPC timestamps mapped exactly onto the app clock); button transitions timed from the raw reports; screen px → window points → source px through the viewport as last drawn. The input probe (`TT_INPUT_PROBE=1`) shows the mapping offset against egui's pointer |
| Pipeline | ✅ one `sketch` operator (Global footprint, evaluated in one call): 240 Hz grid → dead zone → zero-phase One Euro (odd-reflection padded ends) → jiggle → size → lag-compensated resampling through the ClockMap → union window → smoothing of the point (trend-preserving ends) and of the region *as extents around the point*. Frames shown in the last `lag` before the release get no result: the hand never reached them (replaces the planned catch-up, which would invent positions) |
| Hold-to-simulate | ✅ press and hold while paused. A held frame takes the hand at the end of the hold (no lag shift). Steps while holding sculpt frame by frame |
| Live feedback | ✅ raw hand trail (0.5 s), the sketch with the stroke laid over it (path and region), other sketches faint, the selected one bright with its path ±90 frames, crosshair and key hints. In the Select tool, a click on a box selects its sketch. While holding: the pointer hidden, and a 24 pt clear window of raw video at the pointer (a masked second draw of the frame over the overlays; both in Settings; seen in the demo's `1-recording` and `3-nested-live` screenshots) |
| Re-tuning | ✅ every parameter in the Inspector (drag = one undo step); presets Tight / Default / Loose; "use for new sketches". ⏳ raw vs smoothed trail toggle |
| Timeline lanes | ✅ one lane per sketch in tree order (valid / stale coverage), with each stroke's span under selected sketches. The live stroke shows its visited frames and the frames its falloff moves, and its lane scrolls into view when it starts. ⏳ summaries, uncertainty |
| Selecting and commands | ✅ box selection in the Outliner and Timeline; click / Ctrl / Shift selection; a shared right-click menu (enter view, rename, duplicate, delete, select strokes / sketch, select all); keys `X`/`Delete`, `Shift+D`, `A`, `F2` (Delete and Duplicate wait for a stroke to end). The Outliner is a tree with filtering. The Timeline scrolls its lanes (wheel, middle-drag, scrollbar) under a fixed ruler you scrub or click on. Boxes are anchored to the content and scroll the list past its edges; Esc drops them. Lists (and Select All) follow creation order. New names never repeat a live one. Each command is one undo step and tested (`tests/commands.rs`); the demo drives the UI with injected pointer events and checks each step |
| Anticipatory speed | ✅ (the user's idea) on by default (settings version 3), remembered (`tt_core::autospeed::AutoSpeed`). While a stroke records and the video plays, it reads ahead in the parent sketch: the biggest box in the next `look_ahead` s, placed between that sketch's own 20th (calm) and 80th (busy) percentile box sizes, picks a rate in your range (calm → `fastest`, busy → `slowest`, log scale); quickly down, slowly up. Settings shows only the range and the look-ahead; foresight, the hand (with *Calibrate from my recent strokes*) and response times are under Advanced. Q/E while it drives multiply its choice (×1.5 per press, kept). The release restores your rate. The badge shows `auto ×… · busy ahead · ×1.5 yours`; your multiplier flashes, its own changes don't. *(Simplified after hands-on use: the user didn't want to tune it, and "300 pt/s" meant nothing to them.)* DESIGN §8.4 |
| Takes and levels, modifier stacks | ⏳ next |

**Measured:**
- Scripted noisy hand at ¼× (tremor, 250 ms lag) over the sprite path, every frame counted including the first and last: point error median 1.10 px, p95 1.95 px, the sprite inside the region on 100% of frames (`tests/sketch.rs`, held as the regression bar: median < 1.5, p95 < 3.0, ≥ 99.5%).
- The same through the running app (`TT_SKETCH_DEMO=fixtures/sprite_truth.json`: hold, Space taps to play at ¼×, a 1 s pause mid-way, pause and release): median ≈ 1.5 px, p95 ≈ 2.5 px, max < 3 px, 100% containment over 120 frames (three runs). Then an edit (a 0.6 s hold 40 px off the path at frame 180) moves that frame by ≈ 37 px (the hand's tremor and the dead zone take the rest), its neighbours exactly by the falloff curve, and nothing beyond ±12 frames.
- Paused vs playing (`tests/capture.rs`): the same ±20 px jiggle gave a 108 × 74 box while playing but 43 × 36 as a paused edit. Two causes: the jiggle was measured against the responsive point path, which follows part of a jiggle; and the motion union was per stroke, so a one-frame edit had no neighbours to union with. Now the jiggle is measured against a slow reference and the union runs after layering: 132 × 94 while playing, 130 × 102 as a paused edit (88 × 74 for the jiggle alone, without the path's motion). Accuracy unchanged (median 1.10 px, 100% containment); re-tuning a 60 s capture 3.1 ms.
- An adversarial review of the stroke model (4 reviewers + skeptics) confirmed 7 issues, all fixed with tests: writes into a sketch undone mid-stroke, clicks committing edits, quiet holds shrinking the box (now Ctrl = move only), text focus swallowing Space/Shift/Esc during a stroke, one wheel notch both setting falloff and zooming, Alt+A applied after the tool read the selection, and bridging reach rounded up for sub-frame falloffs.
- A review of the selection, commands and settings work confirmed 2 high and 6 medium issues (plus low ones), all fixed, most with tests: timeline clicks were dead (egui clears `press_origin` on the release frame; the demo now clicks the ruler and a lane and double-clicks it), a panic on clips shorter than about width/42 frames, Duplicate copying a sketch twice when one of its strokes was selected too, Delete/Duplicate during a stroke, deleted entities' signals kept in the project file forever, list order scrambled after a video switch (bevy reuses freed entity ids last-freed-first), outliner boxes picking rows out of sight and auto-scroll fighting the user, and the wheel's size/falloff traps.
- Dev runs use `TT_DATA_DIR=<scratch>` and `--target-dir target/bench`, so they never touch the user's projects or the release build they are running.
- Hold-to-simulate: holding still settles to the minimum box (32 × 32); jiggling grows it 3.5×.
- Anticipatory speed (`tests/autospeed.rs`, 1 canvas px = 1 pt, default knobs):
  - with the hand mode on: a still hand, then racing at 1500 pt/s with a ±30 pt jiggle, drops below ×0.5 within 0.1 s and down to the slowest; a still hand from ×0.25 climbs toward `fastest` within seconds;
  - editing a sketch with a 20 px/frame dash at frames 300–330 (the percentile model): ×1.35 while it is beyond the look-ahead, ×0.10 just before it; reading the parent through a child's view does the same;
  - the release (commit or Esc) restores the manual rate exactly; a Q press mid-stroke turns its ×2 into ×1.33 (÷ 1.5), and it keeps driving;
  - the sprite recorded under auto speed (×0.39–×1.01, 279 frames): median 1.63 px, p95 2.70 px (1.51 px median at a fixed ½×).
  - In the app (demo phase 5, from ¼×, the viewport at 0.54 pt per source px; two runs): ×0.25–×1.30 (limited by the hand's speed 58–61% of the time, calm 35–37%, jiggle 4–5%), back to ×0.25 after the release, no flash; ~325 frames, median 2.5–2.6 px, p95 4.8–5.0 px, max 13–17 px at a sharp turn passed at ~1.3× (the ¼× recording: 1.16 px). `comfort` sets that trade. Screenshot `11-auto-speed`.
- The demo's scripted hand saw each frame one app frame late (it logged the frame shown before that frame's advance). Harmless at ¼×, it cost several pixels at 1×; fixed. The ¼× recording went from ≈ 1.5 px to 1.16 px median (p95 2.0 px, max 2.2 px), the nested sketch 0.47–0.50 px.
- Per-stroke lag in real time (`tests/capture.rs`, the hand 0.25 s late at the playback rate, through the tool): median point error 2.33 px at 2× (481 frames) and 1.51 px at ½× (121 frames). Counted as 0.25 s of *video* instead, the same strokes would be off by 109 px and 57 px. Changing one stroke's lag leaves the other stroke's frames bit-identical.
- Re-tuning a 60 s capture: 2.1 ms at 1× (3,600 frames), 1.7 ms at ¼× (release build).

The original plan follows.

**Goal:** follow something with the mouse, get a clean, adjustable path and box, and keep tuning it forever.

1. **Sketch tool** (DESIGN §8.1):
   - press-and-hold capture at the chosen capture speed;
   - every pointer event recorded in the drawn-in view's space;
   - the ClockMap records every transport change (pause, step, scrub).
2. **Pipeline operators** (DESIGN §8.2): Lag, One Euro smoothing plus zero-phase refinement, Dead zone, Jiggle → extent with a critically damped spring, Resample (last-wins / average), Union window, catch-up at release.
3. **Hold-to-simulate while paused** (DESIGN §8.3), including simulate + step.
4. **Live feedback:**
   - raw cursor, filtered cursor, live box (bounded-window re-derivation);
   - the box outline;
   - a faint display of existing captures.
5. **Re-tuning:**
   - a Smoothing panel with presets (Tight / Default / Loose) over the numeric parameters;
   - raw vs smoothed trails;
   - every slider re-derives within the frame.
6. **Takes and levels:** new level vs another take at the same level; robust averaging; uncertainty flags.
7. **Modifier stacks** (DESIGN §9) on any signal: Smooth, Offset (keys), Lag/Lead, Wiggle, enable / influence; plus **Fit to keys** and **Bake**.
8. **Timeline lanes:**
   - capture coverage;
   - uncertain ranges;
   - key diamonds;
   - stale / valid / absent from the summaries.

**Accept:**
- **Moving-sprite fixture:** a sketch at ¼× by a scripted "noisy hand" (ground truth + tremor + 250 ms lag) yields a centre error ≤ X px and a box that contains the sprite on ≥ 99% of frames. X is set by the first measurement, then held as a regression bar.
- **Hold-to-simulate:** jiggling while paused grows the box at that frame, and holding still shrinks it without overshoot.
- **Undo:** a whole capture is one undo step, and re-tuning is undoable per slider release.
- **Speed:** re-tuning a 60 s capture re-derives in ≤ 5 ms.

---

## M4 · Derived views (flagship): core done 2026-09-27

**Status (as built):**

| Item | Status |
|---|---|
| `frame` operator | ✅ fit, hold (a decaying max, zero-phase), zoom and pan damping (zero-phase), dead zone, follow/zoom influence against the parent, zoom limits relative to the parent; the canvas is the widest crop. Zoom lock (default on): one crop size over the whole sketch. ⏳ causal mode, reference frame, a soft zone |
| Views and nesting | ✅ every view maps straight to the source. A stroke drawn in a view records the view's mapping per frame and lives in source pixels, so re-tuning a parent never moves a child (a deliberate change from "children re-derive"). A nested sketch's motion union is measured in its home view |
| Viewport | ✅ `Tab` / `Shift+Tab`, a clickable breadcrumb, rendering through the view (original when magnified, nearest framing outside the sketch), overlays and pointer in view pixels, the view's settings in the Inspector under the sketch. ⏳ an off-frame pattern (off-frame is the background colour) |
| Side by side | ⏳ two viewports with a synced playhead |

**Measured:**
- `tests/view.rs`: a root view keeps the sprite in its central 30% on 100% of 172 frames. Three levels deep, the deepest view does too on 100% of 138 frames (the M4 acceptance, ≥ 99%). A sketch drawn inside view 1 has a median error of 0.68 px vs 1.99 px at level 1. Re-tuning view 1 leaves the child's points identical.
- Zoom from a jittery region (after hands-on use): the crop had been clamped to the raw need after smoothing, so the zoom snapped wherever the region poked out. Now it zooms out ahead (`lead`), back in slowly (`hold`, halving time), and is smoothed by a max filter plus an inner Gaussian that provably covers the need.
  - On the demo's sketch the region's height changes by a median 1.69% (max 7.27%) per frame; the view's zoom by 0.22% (max 0.79%).
  - Synthetic ±25% jitter: 0.03% per frame, the region inside on every frame.
- Zoom lock (after hands-on use: the zoom still wandered with an erratic hand). `FrameParams::lock_zoom`, on by default and for old saves, with a Settings default for new views. `tests/view.rs`: over a region jittering between 50 and 150 px half-height, the locked crop is one size (888 × 499, the unlocked envelope's widest), the region inside on every frame, and a narrower parent still limits it per frame. Nested sketch error (drawn in view 1): median 0.84 px locked vs 0.76 px unlocked, since the locked view magnifies less on most frames.
- A review of M4 (4 reviewers + skeptics) confirmed 6 issues, all fixed with tests where testable:
  - Tab-then-hold re-edited the parent instead of nesting (Tab now clears the selection);
  - a tight parent's zoom limit could crop a child's region (fit now always wins);
  - breadcrumb clicks reached the video (it is now on its own layer);
  - a view snapped when its own sketch was edited (framing changes now ease in);
  - scrubbing during a stroke in a view recorded every frame in between (only playback does now);
  - the nearest-framing lookup rescanned the signal (it is now a clamp).
  Per-view zoom/pan memory came from its unverified list.
- In the running app (`TT_SKETCH_DEMO`, which now also Tabs into the view and records a nested sketch there, saving screenshots under `<TT_DATA_DIR>/screens/`): the view keeps the sprite central on 100% of 120 frames; the nested sketch's error is median 0.50 px, p95 1.34 px (level 1: 1.49 px).

The original plan follows.

**Goal:** see exactly what a sketch sees, sketch again inside it, and go as deep as you like.

1. **`Frame` operator** (DESIGN §10.1): display size = decaying max extent, fit, dead / soft zone, damping (pan and zoom separately; zero-phase or causal), zoom clamp, influence per channel, reference frame.
2. **Views tree:**
   - `ViewOf` relationships;
   - composed source→view transform signals (derived, cached, invalidated through the chain).
3. **Viewport view switching:**
   - breadcrumb (`Source ▸ Sketch 1 ▸ Sketch 1.2`), outliner, keys to go up and down the chain;
   - a crop shader that samples the original at high zoom, with an off-frame pattern.
4. **Editing through a view:** the tools act in view space and store data in its native space; re-sketching a segment inside a view.
5. **Nested sketching:** capture inside view V → child view W, to any depth. Stable when parents are re-tuned: children re-derive, and stale states are shown while that happens.
6. **Side-by-side:** two viewports, one on Source and one on a derived view, with the playhead synced.

**Accept:**
- Nested three levels deep on the sprite fixture, the deepest view keeps the sprite within the central 30% of the display on ≥ 99% of frames.
- Re-tuning a level-1 sketch updates levels 2–3 live, marking stale ranges until re-derived.
- A position picked inside a level-3 view lifts to source space within 0.1 px of the analytic transform.

## M6 · Trackers: started 2026-09-27

The first automatic tracker, built the way the whole design intends: an operator entity whose inputs are a rough pass and a view, run by background jobs (DESIGN §6.2).

**Built (`tt_track`):**
- **The `track` operator.** Inputs `guide` (a sketch, or any box producer), `space` (the view it tracks in; none = the source) and `look` (the patterns to find, DESIGN §6.3). Its output `[x, y, left, top, right, bottom, score, flags]` in source px is a box, so views, overlays and the like take a tracker wherever they take a sketch. `T` tracks the selected sketches from the playhead in the view being looked at. On a selected tracker, `T` re-seeds it at the playhead (moved into the guide's frames). One undo step for all of it.
- **Template strategy.**
  - At the anchor, a template is cut around the guide's point.
  - On every other frame:
    - the prediction is the guide's point plus the last offset;
    - a centre-weighted NCC search runs within the guide's box, with a gentle preference for the prediction;
    - the appearance blends the anchor's look with the last frame's (`adapt`).
  - Below `min_score`, a frame is *lost*: it follows the guide and is drawn red. The tracker re-locks when the look returns.
  - The patch scale comes from the guide's box: `feature` = the fraction of the box that is the subject.
- **Jobs.**
  - Each side of the anchor is a thread with its own ffmpeg.
  - Backward jobs decode keyframe-aligned segments (≥ 64 frames), keep only the guide's region, and track the segment in reverse, so any source GOP works backward. At most 256 patches are kept; a longer GOP is decoded again from its keyframe for each batch.
  - Rendition Auto reads the proxy only where it has a pixel per patch pixel on every frame, on both axes (a proxy's width is rounded to even, so x and y scale separately). It keeps reading the rendition the results so far came from.
  - At most 4 threads decode at once. Cancelled threads count until they stop; jobs held at the playhead for 1 s close their ffmpeg and don't count.
- **What re-tracks** (DESIGN §6.2):
  - Dirt (`Footprint::Radiating(anchor)`) triggers a new plan. The runner compares it with the plan the results came from, and re-tracks from the first frame whose guide box or view map differs, on each side of the anchor. A new anchor or tracking setting re-tracks everything; `follow_playhead` and `center_on_guide` re-track nothing.
  - A restart resumes from the result just before the first frame to redo; a drag re-plans once, when it ends.
  - Results stay on screen as stale until replaced, and while the guide is deleted (undoing the delete brings them back without tracking).
  - A saved input stamp keeps results when a reopened project's inputs come out the same. Only complete results carry it (Forward and Backward trackers too), and it doesn't depend on which rendition was read.
  - Results are saved: stamp and offset changes count as document changes (autosave, save on exit), with no undo step.
  - Redo of `T`, and undoing a tracker's delete, track it again (an operator that comes back recomputes itself).
- **Catch-up mode** (`follow_playhead`): jobs stop at the playhead and continue as it moves, forward and backward.
- **Re-centring on the guide** (`center_on_guide`, on by default): once results stop arriving (finished, or held at the playhead), the path is shifted by the median offset between it and the guide over the frames where the tracker saw the subject (no shift with fewer than 3 such frames). The tracker gives the motion; the rough pass, averaged, gives where the subject is. Without it, every frame would inherit the guide's error at the anchor.
- **UI:**
  - the viewport draws each tracker's box and path (cyan; lost frames red; stale dim);
  - the top bar shows running trackers and their fps (the app repaints while results arrive);
  - the inspector shows progress per side, lost frames, the rendition read and errors, plus a Track button on sketches.

**Measured** (`crates/tt_track/tests/sprite.rs`: the sprite fixture, 1200 frames of 1080p60 H.264 with GOP 250 and B-frames; the guide wanders ~6 px off the truth in a 56 px box; anchor mid-clip, tracked both ways):
- absolute error, re-centred on the guide: **median 0.25 px, max 0.65**, with no lost frames, at ~500 fps (both sides together);
- the motion alone (the anchor's offset removed): median 0.31 px, p95 0.80, max 0.87;
- the rough pass alone is ~1.5 px median;
- synthetic subpixel motion (`tests/template.rs`): median 0.02–0.06 px.

Findings on the way:
- **The sprite fixture's truth was off by up to 1 px:** ffmpeg's overlay on yuv420 puts the sprite on even pixels. `sprite_truth.json` (the in-app demo's truth) needs the same rounding; see the xtask.
- **A plain NCC template lost the sprite whenever it crossed from dark background to bright** (the box always holds background). Centre-weighting the correlation fixed it.
- **Seeded ~2 px differently** (a sketch-built guide, `tests/sprite.rs`), the template slips onto the background for 3 frames near 1044, by up to 35 px, with scores ~0.8, so they don't count as lost. This is a limit of the template strategy (the Lucas–Kanade refinement or a learned tracker should help).

**Looks, the Track tool, validity** (DESIGN §6.3, after hands-on use: "I have no idea what point it's selecting"):
- A tracker is defined by its **looks**: entities with a frame, a rectangle and an optional painted mask. The first look is the seed: the anchor is its frame and the tracker's point there is exactly its centre. Every look is a masked, centre-weighted NCC template; the best one wins per frame (a cursor that changes icon = several looks).
- **The Track tool** (`T`): drag a rectangle (or click for the dashed brush square; the wheel sizes it) inside a sketch; `Shift`+drag adds a look to the selected tracker. The **Look editor** paints masks (Auto / Fill / Invert / Clear). Trackers and their looks are in the outliner; *Re-seed here* adds a look where the tracker is.
- **Validity is a flag:** `flags` marks lost (below `min_score`, now 0.6) and outside (the point left the guide's box). Raw values stay; views and re-centring skip flagged frames; the overlay draws them red. Older 7-channel results are replaced.
- Measured: a placed look tracks at median 0.08 px (max 0.20); a masked cursor arrow over a changing background at ≤ 0.53 px, where the unmasked one gets lost.

**Fixes after hands-on use** ("it's totally missing the white cursor and marking it found", at 0.97 on dark foliage; DESIGN §6.3):
- *Re-seed here* no longer makes a look where the tracker is (that is how foliage became the seed); with no look on the frame it asks for one (the Track tool).
- Scores also require similar contrast (and, painted, brightness): a dim look-alike no longer scores like a white cursor.
- Every look pins its frame; all looks agree on one point (aligned against each other when jobs start), so patches don't make the path jump.
- The search widens from the prediction to the whole guide box when nothing near is good enough.
- With a tracker selected, the Track tool's drag patches it; new looks are masked automatically (a setting).
- Deleting or restoring a look re-tracks.
- On the user's footage (frames 37619–40235): on the cursor 84.6% → 100.0%, at ~90 fps (DESIGN §6.3 table).

**Next:**
- trackers in the timeline (lanes with score and job progress), and Tab into a tracker's view (stabilization);
- unguided trackers (search around the last position and velocity);
- tracker results feeding anticipatory speed's look-ahead;
- a Lucas–Kanade refinement for hard-edged features (NCC peaks lean toward whole pixels);
- colour (chroma) in the match;
- the forward/backward fuse;
- learned trackers (CoTracker3 / TAPNext worker, SAM 2.1) behind the same operator and job protocol.

---

## After the first target (to be re-planned)

| Milestone | Scope |
|---|---|
| **M5 · Keys & curves** | Keys operator as a Track ("human animation is a tracker"), curve editor, dope-sheet editing, finetune-style offset layers |
| **M6 · Trackers** *(started: see above)* | **FrameSource** entities with Auto rendition selection (a mip level per view and model input) and on-demand ½ / ¼ tracking renditions; coarse-to-fine via guide inputs; template matcher (Rust / GPU); learned trackers via a Python worker (v1's CoTracker3 engine; TAPNext as the permissive option) over a narrow job protocol (seeds + view transform in, result chunks out); trackers attach to any view; **reverse** jobs (backward decode by keyframe interval) with a forward/backward fuse; **catch-up-to-playhead** mode; stale-while-revalidate |
| **M7 · Targets & quality** | Combine / Contribution (pushed position, from v1), drift and motion-consistency checks, automatic ends |
| **M8 · Export** | JSON / CSV, AE keyframe clipboard text, Fusion `.setting` (validated in Resolve this time), Nuke `.chan` |
| **Later** | Result caching (content-addressed), audio, zero-copy hardware decode, operator graph view |

---

## Risks

| Risk | Mitigation |
|---|---|
| Open keyframe groups or B-frame reordering break frame exactness in the piped decode | Spike S1 on the real P5 file before building on it. Fallback: ffmpeg `-ss` accurate seek per request, or a proxy-first policy. |
| eframe hides high-rate input | Spike S3. Fallback: run our own winit event loop with `egui-winit` directly. |
| bevy_ecs 0.20 API churn | Pin 0.19.1. Migrate in one dedicated step after 0.20 is final. Keep ECS use behind thin `tt_core` helpers. |
| Scope creep in M2 (the core) | M2's acceptance is tests, not features. Flagship features only use what M2 proved. |
| The transaction audit is too slow | Debug builds only; sampled. |
