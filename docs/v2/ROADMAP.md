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
| Transport and timeline | ✅ rates 0.1–4×, steps, zoomable/pannable timeline that follows the playhead, decode-cache strip |
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
  - deleted entities skipped;
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
| Sketch tool | ✅ `D` arms it. **Auto-key model** (reworked after hands-on use): holding the button records against whatever frame is shown and never touches the transport. Paused, it edits that instant; tap Space while holding to record across frames. Each press → release is a *stroke* on the selected sketch, or a new sketch with nothing selected or with Shift. Each stroke is **one undo step**. A quick click selects the sketch under it and never records. Ctrl+hold moves only (keeps the box size). Esc cancels, Alt+A deselects. If the edited sketch is undone mid-stroke, the stroke starts a new sketch instead of writing into the deleted one |
| Strokes and falloff | ✅ `layer_over`: a stroke replaces the frames it visited (× influence), and frames within its falloff move with its edge offset (Blender's smooth falloff, normalised where edits overlap). Short gaps in new territory are bridged (blocking with holds). The wheel sets the falloff while holding. Stroke falloff and influence are editable afterwards |
| Pointer input | ✅ a raw-input service thread (1 kHz, QPC timestamps mapped exactly onto the app clock); button transitions timed from the raw reports; screen px → window points → source px through the viewport as last drawn. The input probe (`TT_INPUT_PROBE=1`) shows the mapping offset against egui's pointer |
| Pipeline | ✅ one `sketch` operator (Global footprint, evaluated in one call): 240 Hz grid → dead zone → zero-phase One Euro (odd-reflection padded ends) → jiggle → size → lag-compensated resampling through the ClockMap → union window → smoothing of the point (trend-preserving ends) and of the region *as extents around the point*. Frames shown in the last `lag` before the release get no result: the hand never reached them (replaces the planned catch-up, which would invent positions) |
| Hold-to-simulate | ✅ press and hold while paused. A held frame takes the hand at the end of the hold (no lag shift). Steps while holding sculpt frame by frame |
| Live feedback | ✅ raw hand trail (0.5 s), the sketch with the stroke laid over it (path and region), other sketches faint, the selected one bright with its path ±90 frames, crosshair and key hints. In the Select tool, a click on a box selects its sketch |
| Re-tuning | ✅ every parameter in the Inspector (drag = one undo step); presets Tight / Default / Loose; "use for new sketches". ⏳ raw vs smoothed trail toggle |
| Timeline lanes | ✅ one lane per sketch (valid / stale coverage), with each stroke's span under the selected sketch. The live stroke shows its visited frames and the frames its falloff moves. Click to select. ⏳ summaries, uncertainty |
| Takes and levels, modifier stacks | ⏳ next |

**Measured:**
- Scripted noisy hand at ¼× (tremor, 250 ms lag) over the sprite path, every frame counted including the first and last: point error median 1.10 px, p95 1.95 px, the sprite inside the region on 100% of frames (`tests/sketch.rs`, held as the regression bar: median < 1.5, p95 < 3.0, ≥ 99.5%).
- The same through the running app (`TT_SKETCH_DEMO=fixtures/sprite_truth.json`: hold, Space taps to play at ¼×, a 1 s pause mid-way, pause and release): median ≈ 1.5 px, p95 ≈ 2.5 px, max < 3 px, 100% containment over 120 frames (three runs). Then an edit (a 0.6 s hold 40 px off the path at frame 180) moves that frame by ≈ 37 px (the hand's tremor and the dead zone take the rest), its neighbours exactly by the falloff curve, and nothing beyond ±12 frames.
- An adversarial review of the stroke model (4 reviewers + skeptics) confirmed 7 issues, all fixed with tests: writes into a sketch undone mid-stroke, clicks committing edits, quiet holds shrinking the box (now Ctrl = move only), text focus swallowing Space/Shift/Esc during a stroke, one wheel notch both setting falloff and zooming, Alt+A applied after the tool read the selection, and bridging reach rounded up for sub-frame falloffs.
- Dev runs use `TT_DATA_DIR=<scratch>` and `--target-dir target/bench`, so they never touch the user's projects or the release build they are running.
- Hold-to-simulate: holding still settles to the minimum box (32 × 32); jiggling grows it 3.5×.
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

## M4 · Derived views (flagship, ≈1–2 weeks)

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

---

## After the first target (to be re-planned)

| Milestone | Scope |
|---|---|
| **M5 · Keys & curves** | Keys operator as a Track ("human animation is a tracker"), curve editor, dope-sheet editing, finetune-style offset layers |
| **M6 · Trackers** | **FrameSource** entities with Auto rendition selection (a mip level per view and model input) and on-demand ½ / ¼ tracking renditions; coarse-to-fine via guide inputs; template matcher (Rust / GPU); learned trackers via a Python worker (v1's CoTracker3 engine; TAPNext as the permissive option) over a narrow job protocol (seeds + view transform in, result chunks out); trackers attach to any view; **reverse** jobs (backward decode by keyframe interval) with a forward/backward fuse; **catch-up-to-playhead** mode; stale-while-revalidate |
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
