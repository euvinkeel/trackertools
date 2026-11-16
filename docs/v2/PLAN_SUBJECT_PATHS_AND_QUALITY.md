# Plan: subject path editing, then a code quality pass

Written 2026-11-15 on branch `jev-hardening` at `c941a1a`. This is the working plan for one request, in three phases. It is written to be picked up cold, by the user reviewing it or by Claude after a context compaction.

**Status: done 2026-11-15 (all three phases, and press routing from Part C). Not now, by the user's choice: Shift+click membership (D6 option 3) and the other three Part C items.** Answers: D1 pins, D2 yes (local by default), D3 not now, D4/D5/D8 as recommended, D6 entry points 1 and 2 first (Shift+click later), D7 everything on one `dev` branch (only `main` and `dev` left).

**To resume after a compaction:** read section 9 (progress log) first, then the phase it points at. Section 3 is the map of the code this plan relies on.

---

## 1. The request

1. **Features**
   - `P` selects the paint tool, if `P` is free.
   - Smooth a subject's final path by hand in the viewport, like editing a bezier curve: local, small tweaks, instead of switching its constituents off and on to steer it.
   - A setting on the subject that smooths its whole path, recomputed over the entire path, as a sketch's smoothing is.
   - An easy, obvious way to add and remove a subject's constituents.
2. **Code quality inspection pass**: document the app's explicit and implicit state machines (states, invariants, transition tables) in one file; find invalid states and transitions the code can actually reach, with evidence, including through retries and concurrency; prioritize confirmed bugs by impact and group them into issues. Answer: what would a principal architect rewrite first, and what higher-level abstraction would make the problem disappear?
3. **Execute the fixes** from those findings, each with a regression test, keeping the document up to date.

In the code a subject's constituents are called *members* (`tt_core::subject`). This plan uses "member".

### Working rules

- No commits, pushes or branch changes unless the user asks. `scripts/mac_dev_app.sh` is the user's untracked file: leave it alone.
- No subagents without asking first (the user's global rule). The plan assumes Claude does all of it in this session.
- v1 (`editor/`, `cotracker/`, Python) is legacy and out of scope. Only the Rust workspace (`crates/`, about 49,000 lines) is inspected.
- Paid or networked tests (Jev) and real-model tests (CoTracker, TAPNext) stay opt-in and are not run.
- The user's code principles apply to every line written in all three phases:
  1. One function unless composable or reusable.
  2. Inline a value that is used once.
  3. Validate unknown values once, at the boundary that owns them; pass typed values inward; no defensive re-validation of what a type already guarantees.
  4. No single-use helpers extracted in advance.
  5. Before adding complexity for a speculative or very unlikely race or security case: explain the failure, its likelihood and the cost to the user, and get agreement first.

---

## 2. Decisions for the user

Each has a recommendation, which is what the plan below assumes.

| # | Decision | Recommendation |
|---|---|---|
| D1 | **How hand editing of the path works.** (a) *Pins on the path*: drag any point of the drawn path; it becomes a pin the path passes through exactly, and the path bends smoothly toward it within a reach you set with the wheel. Uses the subject's existing offset keys. (b) *Additive bumps*: a separate list of local displacements on top of the keys. | (a). One list of keys, the timeline diamonds and Inspector key list keep working, and it behaves like anchors on a bezier path. |
| D2 | **What dragging the subject itself does.** Today it keys the shown frame only, and a lone key moves the whole path. With pins, every drag (the subject or its path) would be local with a visible reach, and the wheel's top notch would be "the whole path" (today's behaviour). | Local by default, reach ±0.5 s, top notch = whole path. This changes an existing behaviour, so it needs a yes. |
| D3 | **Bridging.** An extra: mark the stretch between two pins as a bridge, where the members are ignored and the path is a clean curve from one pin to the other. This is the direct replacement for switching trackers off over a bad stretch. | Not in the first cut. Add it after pins work if it is still wanted. About 60 lines plus tests. |
| D4 | **Smoothing order.** Smooth the members' motion first and apply pins on top (a pin stays exactly where it was put), or smooth the finished path (pins get smoothed too). | Smooth first, pins on top. |
| D5 | **Smoothing knobs.** One (seconds, for position and angle together), or separate ones for position and angle. | One. |
| D6 | **Where members can be added and removed.** Candidates: (1) a member list in the Inspector with a remove button per member and an *Add member* menu; (2) right-click anything with a point: *Add to subject ▸* and *Take out of ▸*, with no need to select the subject first; (3) with a subject selected, Shift+click a tracker or sketch on the video to toggle it; (4) drag a tracker's lane onto a subject's lane in the timeline. | 1, 2 and 3. Leave 4 out unless asked. |
| D7 | **Branch and commits.** Keep working uncommitted on `jev-hardening`, or a new branch with a commit per phase. | A new branch, one commit per phase, made only when the user says so. |
| D8 | **Depth of Phase 2.** Every state machine gets documented. The search for reachable invalid states can go equally deep everywhere, or deepest where the document or exported data can be damaged (undo and gestures, the tools, the tracker jobs, project and video switching) and lighter on installers and the updater. | Deepest where data can be damaged. |

---

## 3. What the plan relies on (verified by reading)

**Baseline.** `cargo test --workspace` at `c941a1a` on this Mac: 317 passed, 0 failed, 10 ignored, 2 min 9 s. CI also runs `cargo clippy --workspace --all-targets -- -D warnings` (`.github/workflows/macos.yml`); clippy has not been run yet in this session.

**Keys.** The keymap is data: `Keymap::default` in `crates/tt_core/src/input.rs:124-189`, actions in the `Action` enum (`:18-78`). **`P` is not bound, with or without modifiers.** Every action needs a line in `describe()` in `crates/tt_app/src/panels/settings.rs` (the Settings key list is generated from the keymap).

**The paint tool** is the Track tool (`Tool::Track`) with `NewTrackers::method == Method::Paint` (`crates/tt_track/src/lib.rs:88-113`, `:238-249`). The top bar's buttons set the kind and toggle the tool inline (`crates/tt_app/src/panels/mod.rs:295-334`, applied at `:391-396`), and grey a kind out with a link to the doctor when CoTracker is not set up.

**Subjects** (`crates/tt_core/src/subject.rs`):
- `Subject { anchor, turn, half, offsets: Vec<OffsetKey> }` (`:55-64`); `OffsetKey { frame, x, y, angle }` (`:76-82`).
- `push` (`:125-182`) builds the members' motion; `offset_at` (`:186-200`) is linear between keys and held beyond them; `place` (`:203-206`) applies the offset in the subject's turned frame.
- `evaluate` (`:230-240`) has a `Global` footprint: any change recomputes the whole path. Output is 11 channels; 8 to 10 are the pushed state a drag keys from.
- `add_members` and `remove_members` (`:295-314`) are one undo step each. `drag_subject` (`:351-405`) is the Select tool's drag: it only picks the subject at the playhead's frame.
- Everything downstream reads the output signal: views, layers, SpringFocus, and exports (`subject_path`, `crates/tt_track/src/export.rs:129-133`). A change to the output reaches all of them.

**Subject UI.**
- Inspector: `subject_section`, `crates/tt_app/src/panels/inspector.rs:174-262`. Members are listed as buttons that only select.
- Right-click menu: `crates/tt_app/src/panels/menu.rs:281-340`. Adding or removing needs the subject and the points selected together first, which is why it is hard to find.
- Viewport: `subjects()` in `crates/tt_app/src/panels/overlay.rs:207-246` draws the selected subject's path 90 frames either side of the playhead (`PATH_FRAMES`, `:38`; `path()`, `:431-447`), each frame through the view's framing on that frame.
- Timeline: offset keys are purple diamonds on the subject's lane; lanes can already be dragged onto other lanes (`Carry`, `crates/tt_app/src/panels/timeline.rs:331-439`).
- Outliner: members hang under the subject as reference rows (`crates/tt_app/src/panels/outliner.rs:110-191`).

**Smoothing as sketches do it.** `SketchParams::smooth_position` is seconds of video, applied with `gauss_trend` (`crates/tt_core/src/sketch.rs:473-487`, `pub(crate)`), which pads the ends so they are not pulled inward.

**Older projects.** New fields on a saved component need `#[reflect(default)]` (as on `Tracker`, `crates/tt_track/src/lib.rs:286-304`) so files saved before them still load.

**Tests.** Headless: a `Driver` in `crates/tt_core/tests/common` feeds pointer frames; `crates/tt_core/tests/subject.rs` already covers members, drags, undo and save/load. UI tests with egui's test harness exist in `timeline.rs`. `TT_SCENE_DEMO` builds scripted scenes and can save screenshots (`crates/tt_app/src/scene.rs`, `crates/tt_app/src/shell.rs:274-277`) for looking at the result in the real app; how to drive it for a subject still has to be read.

---

## 4. Phase 1: the features

Order: smallest first, the path editing last because it builds on the smoothing. After each step: the new tests, `cargo test --workspace`, clippy.

### 1.1 `P` selects the paint tool

- A new `Action::PaintTool`, bound to `P` in `Keymap::default`. `tt_core` cannot name `tt_track::Method`, hence a dedicated action.
- One function in `tt_track` does what the top bar's click does today: if the Track tool is on with that kind, back to Select; otherwise set the kind and switch to the Track tool. The top bar's inline code is replaced by a call to it, so the button and the key cannot drift apart.
- The handler for the action lives in `tt_app`, where the doctor is reachable: when CoTracker is not set up it opens the doctor, as clicking the greyed button does.
- Also: the line in `describe()`, "(P)" in the Paint button's tooltip, DESIGN §14.
- Tests: `P` looks up to the action; the toggle function from each starting state (Select, Track with another kind, Track with Paint).

### 1.2 Members: add and remove

All entry points call the existing `add_members` and `remove_members`; none adds a second code path for the edit itself.

- **Inspector**: the member buttons become a list. Each row: its icon, its name (click selects it), whether it has a good point on the shown frame, and a remove button. Under the list, an *Add member* menu of everything that can be a member and is not one yet, in tree order.
- **Right-click menu**: on anything with a point, *Add to subject ▸* (each subject it is not in, and *New subject*) and *Take out of ▸* (each subject it is in). The existing select-both items stay.
- **Viewport**: with a subject selected in the Select tool, Shift+click on a tracker's point or a sketch's box toggles its membership and keeps the subject selected (`select_on_click`, `crates/tt_core/src/tool.rs:141-165`). Members get a ring on the video; a hint line says what Shift+click does.
- A subject that would form a cycle (it already contains this subject, directly or through another) is not offered. Today nothing stops it; see lead L4 in Phase 2.
- Tests: Shift+click adds then removes, one undo step each; a cycle is not offered; the Inspector row's remove button, through the UI test harness.

### 1.3 Smoothing the whole path

- `Subject::smooth: f32`, seconds of video, 0 = off, `#[reflect(default)]`.
- In `evaluate`, after `push`: x, y and angle of the pushed path are smoothed with `gauss_trend` at σ = `smooth` × fps. The result is the base that `place` applies offsets to, and it is what channels 8 to 10 carry, so a drag still keys from exactly what is shown. With D4 as recommended a pin is never smoothed away.
- Inspector: a labelled slider (0 to 2 s) in the subject's section; a drag is one undo step, as its offset fields are.
- Export smoothing (`StabilizerDefaults`) is separate and stays as it is; the tooltip says the two add up.
- Tests: `smooth = 0` gives results identical to today's (the existing tests unchanged); a member with jitter gives a path with visibly smaller frame-to-frame acceleration; the ends are not pulled; a keyed frame sits exactly where it was keyed with smoothing on; a `Subject` saved without the field loads with 0.

### 1.4 Editing the path by hand (assumes D1a, D2)

**Model.** The offset keys become pins. Two changes to them:

- **Smooth interpolation.** `Subject::ease`: `Linear` (the default when loading, so saved projects come out as they were) or `Smooth` (new subjects). Smooth is a monotone cubic through the keys per channel, flat at the first and last key and held beyond them: continuous in velocity, and it never overshoots a pin.
- **Local reach.** A drag at frame *f* with reach *r* writes a pin at *f* and, on each side where no key lies within *r*, a guard key at *f − r* or *f + r* holding the offset that was there before the drag. So only frames within the reach move. Each frame of the drag the keys are rebuilt from the keys as they were at the press, by one pure function `(keys at press, f, r, new value) → keys`, so turning the wheel mid-drag moves the guards without leaving strays, and the function is unit-testable on its own.

**In the viewport** (Select tool, subject selected):
- Pins within the drawn path are shown as handles. The path point under the pointer is highlighted with its frame number.
- Press and drag a path point or a handle: the path bends there. The wheel changes the reach while dragging (shown on the path and in the hint line; its top notch is the whole path, which writes no guards). One undo step. The views hold still during the drag, as they do for today's drag.
- Alt+click a handle removes that pin. The right-click menu and Inspector keep *Remove this key*.
- The pointer is converted through the view's framing on the grabbed frame, not the shown frame, because that is how the path is drawn.

**Where.** `drag_subject` grows from "the subject at the playhead" to "the nearest point of the selected subject's drawn path". The nearest-point search is shared by the press (in `tt_core`) and the hover highlight (in the overlay); the 90-frame window constant moves to `tt_core::subject` so both agree. The reach is remembered for the next drag in a session resource.

**Tests.**
- The pure key function: guards only where needed; an existing key inside the reach is the boundary instead; a key at *f* is replaced; whole-path reach adds no guards.
- A drag on the path at a frame other than the playhead: frames outside the reach are bit-identical to before; inside, the path passes through the pointer at *f* and velocity is continuous at the guards; one undo restores everything.
- The same drag inside a view that follows the subject lands where pointed (the run-away case from DESIGN §6.4).
- Smooth never overshoots; Linear results match today's on the existing tests; save and load round-trip both.

**If D3 is a yes:** a `bridge` flag on a key: from it to the next key the path is a cubic from one pinned position to the other, with end velocities taken from the path just outside, and the members are ignored there. Right-click between two pins to set it; drawn differently on the path and the timeline.

### Phase 1 is done when

- All four features work in the running app (checked through a scripted scene and screenshots, or by the user), not only in tests.
- `cargo test --workspace` passes with the new tests; clippy is clean.
- DESIGN §7 (subjects) and §14 (keys) describe the new behaviour.

---

## 5. Phase 2: the inspection pass

Nothing is fixed in this phase. Its output is one document and a decision point.

### Deliverable: `docs/v2/STATE_MACHINES.md`

One file with three parts.

**Part A, the machines.** For each: where its state lives and its class (document, session, derived); its states; its events; a transition table (state × event → next state and effect, with the guard and a `file:line`); its invariants; what other machines it is coupled to.

**Part B, reachable violations.** Each as an issue: what breaks, the impact, the exact steps that reach it, the evidence, a fix sketch with its cost, and a status that Phase 3 keeps current.

**Part C, architecture.** Answers to the two architect questions, each candidate judged by which issues it removes, how much code it deletes or adds, and its risk.

### The machines to document

Found while planning; the list is expected to grow.

*Core (`tt_core`)*
1. The frame loop: set order, and which systems inside one set have no order between them (`app.rs`; the tool systems of different modules all sit in `Set::Tools`).
2. The active tool: Select, Sketch, Track, Draw, and who may switch it (`tool.rs`).
3. Press ownership and click detection: which system claims a press, click against drag (`tool.rs`, and each drag system probing the others).
4. Gestures over the undo history: every owner of an open transaction (subject drag, layer drag, Draw stroke, lifetime and in/out drags, Inspector drags, the look editor, timeline carries) sharing one `History::open` slot (`history.rs`).
5. Undo and redo: stacks, revision, entities disabled rather than removed, signal snapshots.
6. Operator evaluation: valid, stale, absent per frame; dirty ranges; the budget; blocked and held operators; cycle errors (`op.rs`, `signal.rs`).
7. Views holding still during a drag (`view.rs`, `op::Held`).
8. Transport: play, rate, reverse, shuttle, loop, and what restores when playback stops; auto speed driving the rate (`transport.rs`, `autospeed.rs`).
9. A sketch stroke being recorded: start, cancel, a click, its target deleted under it, commit (`capture.rs`).
10. Selection and the active view when what they point at is deleted or undone.
11. Lifetimes and in/out points.
12. Save, load, autosave (`persist.rs`).

*Tracking (`tt_track`)*
13. What a tracker is asked to do: `TrackRun`, paused-on-open, error (`lib.rs`).
14. A tracker job, per tracker and side: planned, queued, waiting for CoTracker, waiting at the playhead, starting, loading, tracking, parked, finished, failed, cancelled; and the runner's bookkeeping across them (`runner.rs`, `job.rs`).
15. The shared CoTracker worker: start, warm-up, streams, idle close, watchdog, failure (`job/learned.rs`, `tt_app/cotracker.rs`).
16. Background learners and Jev asks.
17. The Track tool and the Draw tool (`tool.rs`, `human.rs`).
18. The human layer over automatic results, and off keys (`human.rs`, `off.rs`).

*App (`tt_app`)*
19. Start-up: the doctor window, then the app; restart after a GPU loss or a panic (`setup.rs`, `shell.rs`, `recover.rs`).
20. Opening a video: the request, the media, the proxy build, which copy is shown (`media.rs`, `video.rs`).
21. The project file: new, open, switch, save as, autosave, the session (`project.rs`, `session.rs`).
22. Background installers: the updater, rebuild-from-source, CoTracker and TAPNext setup (`update.rs`, `rebuild.rs`, `cotracker.rs`).
23. Export windows and their render jobs.
24. Panel gestures: timeline (edge, carry, box, mark, scrub), outliner (rename, box), viewport (framing ease, wheel lock, waiting for a frame), layer media loading.
25. The scripted demo and scenes.

### Method

1. For each machine, find every writer of its state by search, and write the table from the code, not from the design document.
2. Attack each invariant. The patterns to try: undo, redo, delete, or a tool switch in the middle of a gesture; Esc at every point; two owners of the same slot; a job's message arriving after it was cancelled, after its tracker was deleted, or after another project was opened; asking again after a failure; a button that starts a thread pressed twice; a thread that panics; a project or video switched while something runs.
3. Confirm. A violation counts as **confirmed** with a failing headless test, or, where only the GUI can reach it, a chain of `file:line` steps with no gap. Anything less is listed as **suspected** and is not fixed without the user's agreement (principle 5).
4. Rank by impact: (1) the document or exported data is wrong or lost; (2) a crash, a hang, a stuck job, a leaked worker; (3) the undo history is damaged; (4) the UI is stuck in a state; (5) cosmetic. Then group related violations into issues.
5. Read the Phase 1 diff and the files the issues implicate against the five principles, and list what breaks them.

### Leads from planning (to verify; none is a finding yet)

- **L1. Undo in the middle of a drag.** `undo` ends the open gesture and pops it (`history.rs:371-383`), but the subject drag, the layer drag and the Draw stroke carry on (`subject.rs:376-404`, `layer.rs:737-805`, `human.rs:496-511`). Each later frame would then be its own undo step, and a Draw stroke could go on writing into a manual dot the undo just removed.
- **L2. Gestures have no owner.** `History::begin` ends whatever was open (`history.rs:106-109`), `end` ends anyone's, and any `edit` made while a gesture is open joins it (`:361-367`). A key-driven edit during a drag (switching a tracker off, marking an in point) would be folded into that drag's undo step.
- **L3. Esc during a Draw stroke.** `apply_tool_actions` takes Esc whenever no sketch stroke is live and switches to Select (`tool.rs:109-119`). The Draw tool's own Esc, which should make the hold never have happened (`human.rs:451`), has no declared order against it (`tt_track/src/lib.rs:889`). If the first runs first, the stroke is committed instead of cancelled.
- **L4. Subject cycles.** `add_members` only refuses the subject itself (`subject.rs:295-304`). Two subjects added to each other both end up with a cycle error (`op.rs:309-316`).
- **L5. Names by count.** `make_subject` and `tracker_count` number by how many exist, so deleting one and making another repeats a name; `capture.rs` already has a helper that avoids this.
- **L6. Repeated shapes.** The name-or-fallback lookup, the kind test by string (`o.kind == "subject"`), and bare channel numbers (`v[7]` for flags, `v[8..10]`) recur across the three crates.
- **L7. Three kinds of keys.** `OffsetKey` (subjects), `Animated` (layers, `layer.rs:59`), `FocusKey` and the off keys each re-implement sorted insert and lookup. Phase 1 adds smooth interpolation to one of them.

### Architecture candidates to judge in Part C

- **One gesture owner.** A single active-gesture record that owns its transaction, with one path for commit and one for cancel that undo, Esc, tool switches and deletes all go through, replacing the per-tool `Option<…Drag>` plus the copied `released || !down` endings. Would remove L1 to L3 as a class.
- **One press router.** One place decides who owns a press, instead of each drag system asking the others.
- **Typed signal layouts.** Named accessors for each output's channels instead of indices.
- **Operator kinds as constants or an enum** instead of strings compared at each use.
- **One keys type** with its interpolation modes, shared by subjects and layers.

Each is adopted only if the evidence from Part B supports it. None is assumed.

### Phase 2 ends with a checkpoint

The user gets: the issues ranked and grouped, which are confirmed, the proposed fix and cost for each, and which architecture changes are recommended and why. Confirmed bugs go ahead in Phase 3. Suspected ones, and any refactor larger than the bug it fixes, wait for a yes.

---

## 6. Phase 3: the fixes

For each issue, highest impact first:

1. Write the regression test and see it fail for the reason the issue states.
2. Fix it, with the smallest change that makes the invalid state unreachable rather than detected after the fact.
3. See the test pass, and run `cargo test --workspace` and clippy.
4. Update `STATE_MACHINES.md`: the transition table and invariants as they now are, and the issue's status.

Then:
- The architecture changes agreed at the checkpoint, each as its own step with the tests green before and after.
- The quality edits from Phase 2's reading, kept to the files already touched and the ones the issues implicated. No repo-wide restyling.

### Phase 3 is done when

- Every confirmed issue is fixed with a test that failed before, or is listed as deliberately not fixed with the user's reason.
- `cargo test --workspace` passes (at least the 317 baseline tests plus the new ones); clippy is clean.
- `STATE_MACHINES.md` matches the code; DESIGN is updated where behaviour changed.
- The final report says what was fixed, what was left and why, and anything that could not be verified.

---

## 7. Commands

Run from the repository root.

| What | Command |
|---|---|
| All tests | `cargo test --workspace` |
| One test file | `cargo test -p tt_core --test subject` |
| Lints, as CI | `cargo clippy --workspace --all-targets -- -D warnings` |
| Fixtures, if missing | `cargo xtask fixtures` |
| The app with a scripted scene | `TT_SCENE_DEMO=1 cargo run -p tt_app` (details in `scene.rs`) |

---

## 8. Risks

- **Phase 1 edits a gesture that Phase 2 audits.** `drag_subject` is one of the machines in lead L1 and L2. Phase 1 extends it in its current shape; if Phase 3 introduces a gesture owner, it is ported then, with its tests as the guard.
- **Changed drag behaviour (D2).** The existing test `dragging_a_subject_keys_its_offset_on_the_shown_frame` encodes today's behaviour and would be updated, not deleted.
- **System order.** L3 depends on an order the scheduler does not promise. A test may pass by the luck of the current order; the fix is to declare the order, and the test must force the bad one.
- **Size of Phase 2.** About 49,000 lines and 25 machines. D8 decides where the depth goes.

---

## 9. Progress log

Update this as work proceeds. One line per step: date, what, result.

- 2026-11-15: Read the design document, the keymap, subjects and their UI, the undo history, the operator graph, the tools, and the outlines of the tracker runner and the app shell. Baseline tests run: 317 passed, 0 failed, 10 ignored. Plan written.
- 2026-11-15: Approved. `dev` made from `origin/try` (main + jev-hardening + video-formats); local `jev-hardening`, `jev-tracker`, `macos-cursor-icons` deleted (all merged). Remote cleanup and push left for the end.
- 2026-11-15: Phase 1 done. 1.1 `P` (`Action::PaintTool`, `tt_track::toggle_track_kind`, shared with the top bar; `setup::paint_tool` opens the doctor without CoTracker). 1.2 Inspector member list + Add member; right-click Add to / Take out of a subject; `subject::can_join` + `op::reads` refuse cycles (lead L4 fixed). 1.3 `Subject::smooth`, `subject::path`. 1.4 `Subject::ease`, `subject::pinned`, `path_frame`, `SubjectReach`, the drag rewritten (`Grab`), Alt+click removes a pin, a click on the path keeps the selection; overlay pins/hover/reach/hint. `TT_SCENE_DEMO=subject TT_SCENE_SHOT=name` screenshots it (checked by eye). Tests: 331 passed (14 new), 0 failed; clippy clean. Shift+click membership (D6 option 3) not done.

- 2026-11-15: Phases 2 and 3. Leads L1-L3 confirmed with failing tests (L1 on the Draw tool was a panic); I4 (a widget gesture left open, holding autosave) found by reading. Fixed with gesture ownership (`f7993ee`): `Gesture` handles, `owning`, `compound`, `cancel` (Esc in one place), `widget_edit` with a per-frame lease. L5 names fixed with `commands::new_name(s)` (`81b7461`), with a Phase 1 slip (a layer under a subject's path) fixed alongside. I5 found: a direct seek left the shuttle running while paused; one `Transport::stop`. `docs/v2/STATE_MACHINES.md` written (12 machines, issues I1-I8, architecture answers). Tests: 340 passed, 0 failed, 10 ignored; clippy clean. L6 and L7 became recommendations (STATE_MACHINES Part C), not changes: no confirmed bug behind them.

- 2026-11-15: The user's answers: Shift+click membership not now; press routing yes (the rest of Part C not now); the dock launcher pulls the checked-out branch; a PR from dev to main. Done: `tool::route_press` (`Press`, `PressOn`), `layer::press_on_layer` gone, test `a_press_goes_to_one_thing_in_a_fixed_order`; `scripts/mac_dev_app.sh` pulls any branch with an upstream (and the app regenerated). Tests: 341 passed, 0 failed, 10 ignored; clippy clean.

**Next step:** none. The PR from `dev` to `main` is open for the user to merge.
