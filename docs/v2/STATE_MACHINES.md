# State machines

Written 2026-11-15 (the inspection pass in [PLAN_SUBJECT_PATHS_AND_QUALITY.md](PLAN_SUBJECT_PATHS_AND_QUALITY.md), Phase 2). It lists the app's explicit and implicit state machines: where each one's state lives, its states, its transitions, its invariants, and what it is coupled to. Part B lists the invalid states the code could actually reach, each with evidence and status. Part C answers the two architecture questions.

Keep it true. When code changes a machine, change its table here. When an issue's status changes, change it in Part B.

Conventions:
- **Class**: *document* (saved and undoable), *session* (remembered, not undoable), *derived* (recomputed, never saved).
- **Owner**: the code allowed to write the state. Anything else writing it is a bug.
- `file:line` references are to `dev` at the time of writing; search for the function name if lines have moved.
- Depth follows decision D8: deepest where the document or exported data can be damaged (undo, tools, tracker jobs, projects), lighter on installers and the updater.

---

## Part A. The machines

### A1. The frame loop (`tt_core::app`)

Not a state machine of its own, but the order every other machine relies on.

| Phase | Runs | Writes |
|---|---|---|
| `Set::Input` | click detection, pointer trail, ending widget drags not drawn last frame | `PointerFrame.click`, `PointerTrail`, `History.widget` |
| `Set::Transport` | transport actions, then the playhead advance | `Transport` |
| `Set::Tools` | the tools (see A3); **no order between modules' systems** | gestures, document edits |
| `Set::Intents` | key and button actions (history, views, marks, tracks, off switches) | document edits, `ActiveView`, `Selection` |
| `Set::Invalidate` | graph rebuild, dirty propagation | `OpGraph`, `Dirty` |
| `Set::Evaluate` | cheap operators within 4 ms | outputs |
| `Set::Jobs` | tracker runner, then human-layer composition | tracker outputs, `TrackStatus` |
| `Set::Media`, `Set::Prepare` | decode requests, derived panel data, project autosave | |
| egui pass | panels draw and edit (widget drags, timeline drags, menus) | document edits through `widget_edit` |
| `PostUi` | latency-critical transport actions (scrubbing) | `Transport` |

**Invariant:** no machine may depend on the order of two systems in the same set from different modules unless the order is declared. Issue I3 broke this; the fix removed the dependency rather than adding an order.

### A2. Undo gestures (`tt_core::history`)

**State:** `History { undo, redo, open: Option<Open { gesture, t, owner_editing }>, gestures, widget: Option<(Gesture, drawn)> }`. Class: session.

**States:** *Idle* (no open gesture) · *Open(g)* (a transaction collecting gesture g's steps) · *Open(g), owner editing* (inside `owning(g, …)`) · for widgets, *drawn* or *not drawn* since the last frame began.

| From | Event | To | Effect |
|---|---|---|---|
| Idle | `begin(label)` | Open(g) | new number g |
| Open(f) | `begin(label)` | Open(g) | f ends (pushed if it has steps); f's owner sees `!is_open(f)` and stops |
| Open(g) | `edit` inside `owning(g)` | Open(g) | steps join g |
| Open(g) | `edit` outside `owning(g)` (a key, a menu, anything else) | Open(g) | g's steps so far are pushed, then the edit is pushed, then g goes on empty: three steps in order |
| Open(g) | `end(g)` | Idle | pushed if it has steps |
| Open(g) | `end(f)`, f ≠ g | Open(g) | nothing: only g's handle ends g |
| Open(g) | `undo` | Idle | g is pushed, then popped and undone; its owner stops |
| Open(g) | `redo` | Idle | g is pushed (which clears redo), so nothing is redone |
| Open(g) | Esc (`cancel`, from `tool::apply_tool_actions`) | Idle | g's steps are undone; nothing goes on redo; its owner stops |
| Open(g) | a project loads (`clear`) | Idle | everything is forgotten; owners stop |
| Idle | `edit` | Idle | pushed |
| widget drawn | a frame begins (`Set::Input`) | widget not drawn | |
| widget not drawn | a frame begins | Idle | the widget's gesture ends |

**Invariants**
1. Every step in a transaction was recorded in the order it was applied, so undoing transactions in reverse order restores every value.
2. Only g's owner adds to g.
3. An owner never edits under a gesture that is closed. It checks `is_open` each frame and stops.
4. An open gesture is either renewed by its owner every frame (tools while the button is held; widgets while drawn), or it ends within a frame.

**Owners:** the subject drag (`subject.rs` `drag_subject`), the layer drag (`layer.rs` `drag_layer`), the Draw stroke (`human.rs` `draw_tool`), widget drags (`history::widget_edit`: Inspector fields, the subject's smoothing, focus and layer inspectors, sliders, the look editor, the brush tab, the timeline's lifetime edges and in/out marks), and single compound steps (`history::compound`, used by `add_tracker_in`).

**Coupled:** autosave and the tracker runner both wait while any gesture is open (`project.rs:311`, `runner.rs:791`). Invariant 4 is what keeps them from waiting forever.

### A3. Tools and presses (`tt_core::tool`, `capture`, `subject`, `layer`, `tt_track::tool`, `human`)

**State:** `ActiveTool` (session); per tool, the press in progress: `LiveCapture` (sketch), `SubjectDrag`, `LayerDrag`, `TrackTool.drag/stroke`, `DrawTool.stroke`, `ClickTracker` (all derived).

**Active tool**

| From | Event | To |
|---|---|---|
| any | `Action::Tool(t)`, t ≠ current | t |
| t | `Action::Tool(t)` | Select |
| any | `P` (`Action::PaintTool`, CoTracker set up) | Track (paint kind), or Select if already that |
| any | `P`, CoTracker not set up | unchanged; the doctor opens |
| any | a top-bar kind button | as `P`, for that kind (`tt_track::toggle_track_kind`) |
| any | Esc, no sketch stroke, no gesture open | Select |
| any | Esc, a gesture open | unchanged; the gesture is cancelled (A2) |

**A press in the Select tool.** Systems in `Set::Tools` each decide whether a press is theirs. The precedence is implicit, by each system checking the others' pick functions.

| Press lands on | Taken by | Click (no move) | Drag |
|---|---|---|---|
| a layer's picture, not selected | layer drag | selects the layer | moves, scales, turns it |
| the selected layer | layer drag (handles) | | |
| a subject's point (playhead frame) | subject drag | selects it | bends its path there (A4) |
| the selected subject's drawn path | subject drag | keeps it selected; Alt removes the nearest key | bends its path there |
| a tracker's point, a SpringFocus, a sketch's box | nothing (click only) | selects it | nothing |
| empty | nothing | clears the selection | nothing |

**Per-press machine** (the same shape in every drag tool): *Idle → Pressed (grab recorded) → Dragging (gesture open, from the first move beyond 4 screen points) → Idle* on release, tool change, or the gesture closing under it (undo, Esc, another gesture). Pressed → Idle without a move is a click.

**Invariant:** a press has at most one owner. It holds today because each drag system checks `press_on_layer` and the subject pick before taking a press, not because of a router (see Part C).

### A4. A subject (`tt_core::subject`)

**State:** `Subject { anchor, turn, half, offsets, smooth, ease }` and `Inputs` (member slots). Class: document. Output: derived (operator, `Footprint::Global`).

| Event | Effect |
|---|---|
| members' outputs change | the whole path re-evaluates: `path()` = `push` → smooth (`smooth` s) → offset keys (`ease`) |
| `add_members` / `remove_members` | one undo step; refused for anything that reads the subject (`can_join`, `op::reads`) |
| drag (A3) | keys = `pinned(keys at press, ease, key at f, reach)`, recomputed from the press every frame, one gesture |
| wheel during a drag | `SubjectReach` ±1 notch (session); keys recomputed |
| Alt+click on a key | `remove_offset_key` (one step) |

**Invariants:** keys stay sorted by frame with unique frames (`pinned`, `set_offset_key` insert in place). Frames outside a drag's reach are bit-identical before and after (`pinned` guard keys; tested). Nothing that reads the subject is one of its members (no cycle).

### A5. Operator evaluation (`tt_core::op`, `signal`)

**Per output frame:** *absent* · *valid* · *stale* (kept for display, recompute pending). **Per operator:** `Dirty` ranges · blocked (an input still dirty) · held (`op::Held`, views during a drag) · `OpError` (cycle or evaluation error).

| Event | Effect |
|---|---|
| input output changed on frames R | dependents' `footprint.map(R)` marked stale and dirty |
| params changed | own whole extent dirty |
| `Inputs` changed | graph rebuilt; own extent dirty |
| entity disabled or enabled (undo, delete) | readers recompute; an enabled operator recomputes itself |
| Evaluate within budget | dirty ∩ demanded frames recomputed, front to back; the rest carries over |
| job kind (trackers) | left to the runner (A7) |
| cycle | every operator in it gets `OpError("part of a cycle")`; removed when the cycle is |

**Invariants:** a frame is valid only if no input it depends on changed since it was computed. Dependents of an operator with dirty frames wait (blocked), so they never compute on half-updated inputs.

### A6. Views holding still (`tt_core::view`)

*Free → Held (from a tool asking with `HoldViews::ask`, every frame of a drag) → Free* when no tool asks in a frame. Held views keep their dirty frames (`op::Held`) and catch up when freed. **Invariant:** a view never moves under the pointer during a drag (the run-away bug in DESIGN §6.4).

### A7. Tracker runs and jobs (`tt_track::lib`, `runner`, `job`)

**What a tracker is asked** (`TrackRun`, document but not undoable; `PausedOnOpen`, derived):

| From | Event | To |
|---|---|---|
| (new) | made | `NewTrackers.run` (Paused by default; Jev always Paused) |
| any learned (CoTracker, Paint, TAPNext) asked to run | project opens | effectively Paused (`PausedOnOpen`), saved value kept |
| any | `set_run(r)` | r; `PausedOnOpen` removed; a failed tracker asked again re-plans |
| Jev running | its last job finishes | Paused (asking costs money) |

**A job, per tracker and side** (`TrackJobs.running`, derived; `Waiting`, `job::Phase`, `Shared` atomics):

| From | Event | To |
|---|---|---|
| none | the plan has frames to do on that side, run allows it, a slot (`MAX_JOBS`) and for CoTracker a worker (`cotracker_jobs`) | Starting |
| none | no slot | Waiting (queued) |
| none | CoTracker, worker taken | Waiting for CoTracker, or at the playhead |
| Starting | CoTracker worker loading | Loading |
| Starting, Loading | first frames | Tracking |
| Tracking | catch-up limit reached | waiting at the limit; parked after 1 s (decoder closed) |
| parked | limit moves | Tracking |
| any | `Msg::Finished` | none |
| any | `Msg::Failed`, or the thread ends without saying | none; `OpError`; basis failed |
| any | tracker deleted, video changed, plan changed under it, run no longer wants that side, a held CoTracker job while another wants the worker | none (the entry is dropped, which cancels the job) |

**Invariants**
1. Results reach the document only through the receiver in the job's own entry. Dropping the entry cancels the job and drops its messages, so a cancelled job, a deleted tracker or another video never receives stale results. Verified by reading `drain` (`runner.rs:649`) and `run_trackers` (`:570-604`).
2. Frames whose inputs changed while a job ran are written stale, not valid (`drain`, `outdated`).
3. Re-planning waits while a gesture is open (one plan per drag, not one per frame).

### A8. Sketch strokes (`tt_core::capture`)

*Idle → Live (press, Sketch tool, video open) → committed (one undo step) / cancelled (Esc) / discarded (a click, or nothing visited).* While live, the target sketch can be deleted or undone under it: the stroke then starts a new sketch (`capture.rs:259-266`). Undo during a stroke undoes the step before it; the stroke commits at release on its own. **Invariant:** a stroke is one transaction, made at release, so nothing it does mid-stroke is in the history.

### A9. Transport (`tt_core::transport`)

**State:** `playing`, `rate`, `reverse`, `shuttle: Option<rate before>`, `looping`. Class: session.

| From | Event | To |
|---|---|---|
| paused | Space / K | playing forward at `rate` (from the start if at the end) |
| playing | Space / K, a seek, a step, the end (not looping), opening a video | paused, through `stop()`: forward, the rate before the shuttle back |
| any | L / J | playing at 1× that way (or the shuttle's own direction ×2, up to 8×); `shuttle = Some(rate before)` |
| any | Q / E, the speed menu | rate changes (auto speed's multiplier while it drives) |

**Invariants:** `shuttle.is_some()` ⇒ `playing`. Paused ⇒ forward. Every stop goes through `Transport::stop`. Issue I5 broke this.

### A10. Selection, active view, lifetimes, in/out marks

- **Selection** (session, snapshotted by every transaction). It can hold disabled entities after an undo. Consumers filter with `get_entity` or `Disabled` checks; entity handles carry a generation, so a stale handle finds nothing after a project switch.
- **Active view** (session): a deleted view hands the viewport to its nearest live ancestor (`prune_active_view`).
- **Lifetimes** (`Span`, document) and **in/out marks** (`ProjectMeta`, document): edited by timeline drags as widget gestures (A2), by menu commands, and by `I` / `O` keys. **Invariant:** in ≤ out (marking one past the other clears the other).

### A11. Projects and autosave (`tt_app::project`, `tt_core::persist`)

*No project → Open(path, saved revision)*. Autosave: the document has changed (`History::revision` past the saved one), 1.5 s since the last change, and no gesture open. New, Save as, Open and Switch save the current project first, then `clear_document`, which despawns document entities and clears the signals, history, selection and metadata, then load. A file that can't be read is copied aside before the empty document could overwrite it. CoTracker trackers are paused on open (A7).

### A12. Background installers and services (`tt_app`; lighter, per D8)

| Machine | States | Notes |
|---|---|---|
| Updater (`update.rs`) | Idle → Checking → UpToDate / Available / Failed; Available → Downloading → Ready → Restarting | state behind `Arc<Mutex>` shared with its thread; one check or download at a time (`busy`) |
| Rebuild (`rebuild.rs`) | Idle → Running → Done / Failed | the same pattern |
| CoTracker setup (`cotracker.rs` `Step`) | Idle → Runtime → Uv → Python → Torch → Packages → Code → Model → Test → Done / Failed | `busy()` guards a second start |
| CoTracker early start (`EarlyStart`) | not tried → tried (running or failed, notice shown once) | not after a GPU-loss restart |
| Proxy build (`media.rs` `ProxyState`) | NotNeeded / Building → Ready / Failed | per opened video (`generation`) |
| Layer media (`layers.rs` `Load`) | Loading → Ready / Failed | per file, a thread each |
| Recovery (`recover.rs`) | running → device lost or panic → restart (not twice in 5 min, not in the first 10 s) | |
| Doctor (`setup.rs`) | before the app (window) / inside it (shown or not) | |

Not inspected deeply for reachable invalid states (D8). Their threads report through shared state, and the UI only reads it.

---

## Part B. Reachable invalid states (issues)

Ranked by impact: (1) document or exported data wrong or lost, (2) a crash, hang or stuck job, (3) undo history damaged, (4) UI stuck, (5) cosmetic.

### I1. Undo during a drag: crash, and an undo step a frame. **Fixed** (`f7993ee`)
- **Impact:** 2 (crash) and 3.
- **Breaks:** A2 invariants 2 and 3.
- **Reached by:** hold the Draw tool with nothing selected (a new manual dot), press Ctrl+Z. The undo removes the dot and its signals; on the next frame the stroke writes to them, and `Tx::signal` panics (`expect("signal exists")`). In a subject or layer drag, Ctrl+Z ended the gesture, the drag went on writing, and each later frame was its own undo step.
- **Evidence:** tests `undo_during_a_draw_stroke_ends_it` (panicked before the fix) and `undo_during_a_drag_ends_it` (keys were still written).
- **Fix:** gestures have owners (A2). Undo closes the open gesture and its owner stops.

### I2. A key's edit during a drag joined the drag's undo step. **Fixed** (`f7993ee`)
- **Impact:** 3. Undoing the drag silently undid the other edit too: an in point, a tracker switched off, a view made by Tab.
- **Evidence:** `an_edit_during_a_drag_is_its_own_undo_step` (the in point was lost with the drag before the fix); `an_edit_from_elsewhere_splits_a_gesture`.
- **Fix:** an edit not made as the gesture's owner splits it, giving three steps in order.

### I3. Esc during a Draw stroke committed it instead of cancelling. **Fixed** (`f7993ee`)
- **Impact:** 3.
- **Reached by:** `tool::apply_tool_actions` and `human::draw_tool` are both in `Set::Tools` with no declared order. When the tool switch saw Esc first it left the tool, and the Draw tool then ended (committed) the stroke.
- **Evidence:** `esc_during_a_draw_stroke_cancels_it` failed under the order the scheduler chose.
- **Fix:** Esc cancels the open gesture in one place (`history::cancel`), whatever the order; the Draw tool's own Esc code is gone. Subject and layer drags gained Esc-cancel with it.

### I4. A widget drag whose widget stopped being drawn left its gesture open. **Fixed** (`f7993ee`)
- **Impact:** 1. Autosave waits while a gesture is open (`project.rs:311`), and so does re-tracking (`runner.rs:791`).
- **Reached by:** drag a value in the Inspector (a subject's offset), and while holding it press Alt+A (deselect). The section is no longer drawn, egui never reports `drag_stopped` for it, and `History::open` stays set until something else begins or ends a gesture. Meanwhile nothing is autosaved and trackers stop re-planning; a crash in that window loses the work.
- **Evidence:** reasoning from `file:line` (egui reports `drag_stopped` only for widgets it draws); the mechanism is tested by `a_widget_drag_that_is_no_longer_drawn_ends`. Not reproduced through the GUI. Status: confirmed by reading.
- **Fix:** widget drags renew their gesture every frame they are drawn (`widget_edit`); one not drawn for a frame is ended in `Set::Input`.

### I5. A seek outside the action handler left the shuttle running while paused. **Fixed** (`2168d92`)
- **Impact:** 4. After Shift+I or Shift+O during J/L shuttling, the next Space played backward at up to 8×. Opening another video carried the shuttle's speed and direction over.
- **Breaks:** A9 (`shuttle` ⇒ playing; paused ⇒ forward).
- **Evidence:** `a_direct_seek_ends_the_shuttle` gave `(paused, reverse, 4×, shuttle set)` on the old code.
- **Fix:** one `Transport::stop` that every stop goes through: seek, toggle, the end, opening a video.

### I6. New trackers, subjects, SpringFocuses and manual dots repeated live names. **Fixed** (`81b7461`)
- **Impact:** 5, with an edge into 1: exported `.setting` files are named after them (`menu.rs` `copy_stabilizer`), so a second "Tracker 2" export overwrote the first's file.
- **Reached by:** make two, delete the first, make another: it was named after the count, which a live one already had.
- **Evidence:** `new_subjects_never_repeat_a_live_name`.
- **Fix:** `commands::new_name(s)`, as sketches and strokes already used.

### I7. A subject could be made a member of something that reads it. **Fixed** (`a572f45`)
- **Impact:** 4. Both operators were stuck with "part of a cycle", with no way out except removing the member.
- **Evidence:** `a_subject_never_takes_a_member_that_reads_it`.
- **Fix:** `subject::can_join` with `op::reads`; the UI offers only what can join.

### I8. A click on the selected subject's path under a layer's picture kept the subject selected. **Fixed** (`81b7461`)
- **Impact:** 5. Introduced in Phase 1 and caught here: the layer drag had taken the press, but the click didn't select the layer.

### Suspected, not fixed (each needs the user's agreement first, per principle 5)

- **S1. Undo of a stroke's target during a live sketch stroke.** Handled: the stroke starts a new sketch (A8). Listed only to record that it was checked.
- **S2. A disabled (undone) entity re-enabled by redo after a project switch.** History is cleared on load, so nothing can redo into the new document. Checked; no issue.
- **S3. Press precedence depends on each drag system calling the others' pick functions** (A3). No wrong owner was found, but adding a fourth drag tool means editing three systems. This is architectural, not a bug; see Part C.

---

## Part C. Architecture

### What would a principal architect rewrite before starting?

1. **Gesture ownership.** *Done in this pass.* The undo history had one global "open transaction" slot that anyone could fill, end or join. Every drag tool re-implemented begin and end around it, and none could tell when its gesture had been closed under it. I1 to I4 were all symptoms of that one gap. A `Gesture` handle with an owner, plus one place that handles undo, redo, Esc and widgets disappearing, removed the whole class. It also deleted the Draw tool's own Esc code and several `in_gesture()` guesses in the panels.
2. **Press routing.** *Recommended, not done.* Six systems read `PointerFrame` and each decides whether a press is its own by calling the others' pick functions (`subject.rs`, `layer.rs`, `tool.rs`, `capture.rs`, `tt_track::tool`, `human.rs`). One router in `Set::Input` that hit-tests in a declared order and hands the press to one owner would make the precedence in A3 explicit, and the "no order between modules" rule (A1) moot for presses. This is larger than any bug found, so it waits for a yes.

### What higher-level abstraction would make the problem disappear?

- **A gesture is a first-class value** (done). Owning the transaction, the cancel path and the views' hold behind one handle is what made undo, Esc and widget leaks stop being per-tool problems.
- **Typed signal layouts** (recommended). 21 places read channels by number (`v[7]` for flags, `v[8..10]` for a subject's pushed motion). One typed view per layout (`TrackFrame { pos, bbox, score, flags }`, `SubjectFrame { …, pushed }`) would turn a wrong channel index into a compile error.
- **Operator kinds as types, not strings** (recommended). 54 string comparisons in 22 files (`o.kind == "subject"`). A typo there gives silent misbehaviour, not a compile error. A marker component per kind (`With<SubjectOp>`) would also make the queries cheaper.
- **One keyed-value type** (recommended). `OffsetKey` (subjects), `layer::Key` (`Animated`), `FocusKey` and `TrackerOff` each re-implement sorted insert, lookup and interpolation. A generic `Keys<T>` with its interpolation modes (Phase 1 added `Ease` to one of them) would give every keyed thing pins, easing and reach for free.

None of the recommended items fixes a confirmed bug, so none was done without agreement.
