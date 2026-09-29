# Spike S1: frame-exact decode via an ffmpeg subprocess

2026-09-27 · harness: `crates/tt_media/examples/s1_decode.rs` · code: `tt_media::{index, ffmpeg}`

**Question.** Does indexing with `re_mp4` and decoding with an ffmpeg subprocess give frame-exact random access on real recordings, including B-frames, open GOPs, VFR and HEVC? How fast is it?

**Method.**
1. An independent ground truth: one full sequential decode per file (`ffmpeg … -fps_mode passthrough -pix_fmt nv12 -f framemd5`).
2. Our path decodes a set of targets:
   - the ends of the video;
   - each side of 7 keyframes (where exactness usually breaks);
   - seeded random frames;
   - a sequential run.

   Each NV12 frame's MD5 is compared to the reference.
3. On the fixtures, we also read the burned-in binary barcode and check it against the grid index our frame grid predicts.

## Results

| Clip | Frames | Exactness | Seek median / max | Raw sequential |
|---|---|---|---|---|
| P5 recording (3 GB, 1080p60 H.264, B-frames, GOP 250) | 69,470 | **62/62 + 1,200 sequential exact** | 192 / 288 ms | 657 fps |
| Same, `-hwaccel cuda` | | exact | 437 / 578 ms | 600 fps |
| Fixture: H.264 GOP 250 | 600 | exact | 131 / 395 ms | — |
| Fixture: H.264 open GOP | 600 | **2 wrong before the fix**, exact after | 116 / 174 ms | 623 fps |
| Fixture: VFR (86 grid frames dropped) | 514 / 601 grid | exact; all 601 grid slots show the right frame | 151 / 228 ms | — |
| Fixture: HEVC GOP 250 | 600 | exact | 232 / 536 ms | — |

- **Index time:** `re_mp4` reads only the moov box: 11 ms for the 69,470-frame P5 file (target ≤ 2 s).
- **Open-GOP bug, found and fixed.** The frames presented just before a keyframe but *decoded after* it ("leading" frames) reference the previous GOP. ffmpeg's seek lands on the later keyframe, cannot decode them, and silently returns the next frame.
  - `VideoIndex::leading_frame_start` detects such frames from decode order.
  - `FrameStream` then input-seeks to the previous keyframe and trims with an output-side `-ss`, still inside ffmpeg, so skipped frames never cross the pipe.
- **NVDEC** is bit-exact with software decode but slower through a pipe: device initialization adds ~200 ms per spawn, and frames still come back to system memory. It is only worth it in-process with frames kept on the GPU.

## Conclusions

1. **Adopt** `re_mp4` index + ffmpeg subprocess (`-ss` accurate seek, `-fps_mode passthrough`, NV12 rawvideo) as the M1 decode path. It is exact, and there is no FFmpeg linking, no bindgen, no build dependencies.
2. **Sequential decode (~650 fps at 1080p) covers** playback (60 fps), tracker jobs and proxy builds.
3. **A spawn per seek costs ~100 ms before any decoding** (process start + moov parse), plus decoding from the keyframe. That is too slow for interactive stepping and scrubbing on its own. M1 therefore needs:
   - **a frame cache** around the playhead (NV12, ~3.1 MB per 1080p frame; the 2 GB default holds ~660 frames ≈ 11 s);
   - **GOP-granular backward fill:** when the playhead nears the start of the cached window, decode the previous GOP in the background, so backward stepping never waits in steady state;
   - **the scrub proxy** (GOP 12) and a nearest-cached-frame display while scrubbing.
4. **Revisit in-process decoding (libav) if scrub latency on the proxy still feels slow.** It removes the ~100 ms spawn floor, but needs libclang + FFmpeg dev libraries (new installs). The decoder sits behind `FrameStream`'s small interface, so a backend swap stays local.

**Open:** seeking straight *onto* a P5 keyframe measured slower (median 245 ms) than seeking 1–30 frames past one (128 ms). ffmpeg may be landing on the previous keyframe. This is harmless for correctness; look at it if seek latency becomes critical.
