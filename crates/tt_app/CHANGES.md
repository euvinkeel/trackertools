## Not released yet
- Settings > Updates shows what changed in each version, and what is not released yet.
- Settings > Build from your code shows the newest branch on GitHub. Click Switch to it, then Build and restart, to try the latest changes.

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
