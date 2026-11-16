## 0.5.0
- Subjects: drag the selected subject's path anywhere on the video to bend it there. Only the frames near it move; the wheel while dragging sets how far (its last step moves the whole path). The pins you make show on the path; Alt+click a pin removes it. New subjects ease in and out of their pins.
- Subjects: Smooth its path (Inspector) smooths the subject's motion over the whole path. Your pins stay where you put them.
- Subjects: the Inspector lists the members, each with a button to take it out, and Add member. Right-click a tracker or sketch: Add to a subject, Take out of a subject.
- Subjects use a tracker's red (lost) frames too. To leave a tracker out somewhere, switch it off there (H).
- Sketches switch off and on with H, as trackers do. Where a sketch is off, it does not move its subjects.
- Manual dots: drag the selected dot's path to bend it, as a subject's.
- Right-click any sketch, tracker, subject or SpringFocus: Bake into a manual dot. The dot keeps that path, and does not change when the original does.
- SpringFocus: Always on the spring. It follows its target on the spring all the time, like a camera on a spring.
- Paint trackers: P turns on the paint tool. Dots per paint is a setting in the Inspector. New paint trackers track dots along your strokes, so you see where each dot is. Alt+brush on a paint erases it; between paints, it cuts the dots you brush over until the next paint. The hint says what Alt does.
- New trackers go on the whole video, unless a sketch is selected. This works for every kind, paint and TAPNext too.
- Top bar > Export: the video as you see it, with what you attached on top, or only what you attached, on transparency.
- Tracking target video: a transparent background, to put over the footage in your editor.
- Timeline: fold a lane's lanes like comment threads (its triangle, or the line beside them). Lanes for views are hidden; "views" in the timeline's header shows them.
- On a selected path, the point at the playhead (or under the pointer) and the path near it are brighter.
- A long video name no longer pushes the top bar's buttons off the window.
- Fixed: Ctrl+Z while drawing a new manual dot closed trackertools.
- Fixed: Ctrl+Z or Esc during a drag now stops the drag. A key you press during a drag is its own undo step.
- Fixed: after Shift+I or Shift+O while shuttling with J or L, playback went on backward or fast.
- Fixed: a new tracker, subject, SpringFocus or manual dot could get the same name as one that is still there.
- Open ProRes videos (ProRes LT, 422, HQ, 4444, Proxy) and MKV videos. The Open dialog shows MKV files.
- Template trackers: add the cursors on this computer (Windows' own, a Roblox install's) in the Inspector, under Cursor icons. The tracker matches each as one more look, so it keeps following the cursor when it turns into one of them. Each pack's size is found from the looks, or set by hand. Add only the pack your video shows.
- In a narrow window, the top bar wraps onto a second line and the Inspector's rows wrap, instead of running off the edge.
- Cursor trackers: brush loosely over the mouse cursor on a few frames. It learns what the cursor looks like and finds it on every frame. Shift+brush starts a new pattern for another shape of the cursor (a hand, an I-beam); keys 1–0 pick one. Each pattern shows in its colour, and what it learned shows under the brush and in the Inspector. Frames where nothing matches are marked red. In the Inspector, each paint shows as a picture with where its pattern was found in it (red if it was left out, and why), each pattern has its own match score, and a setting says how far from the cursor's middle paints may be.
- Paint trackers: a large paint with gaps in it no longer counts as its whole rectangle.
- Settings > Updates shows what changed in each version, and what is not released yet.
- Settings > Build from your code shows the changes ready to try (the try branch) and what they add. Click Switch to try, then Build and restart. It also shows the newest branch on GitHub.

## 0.4.0
- Paint trackers: brush over the thing to follow. CoTracker follows many points on it. Paint again on other frames: only the points that stay on the paint from one paint to the next count. Alt+brush erases. Dots on the video show which points count.
- TAPNext trackers (experimental): Google DeepMind's point tracker. Set it up in the doctor.
- Switch a tracker off where it goes wrong (H), and on again where it is good. Subjects use the other trackers there.
- Right-click on the timeline: track the selected trackers forward, backward or both ways from the playhead, or pause them.
- Drag paints, reset points or looks on the timeline onto another tracker to move them there.
- A changed paint or reset point re-tracks only around it, not the whole tracker. Moving stripes on the timeline show what is tracked again.
- CoTracker starts when a video opens, so the first tracker starts at once. Several CoTracker trackers share one engine.
- Layers show in the stabilized export and its preview.
- trackertools runs on Macs with Apple silicon.

## 0.3.1
- Layers (pictures, GIFs, clips) show in the stabilized export.
- Releases build faster.

## 0.3.0
- Layers: attach pictures, GIFs and video clips to anything that is tracked.
- SpringFocus: one point that moves smoothly from one tracked thing to the next.
- Views hold still while you drag on the video.
- Several CoTracker trackers can track at the same time.
