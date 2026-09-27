// Frame-accurate display on top of a <video> element.
//
// Seeks are coalesced: while one is in flight, further requests only update
// the target, and the next seek is issued when the current one lands. The
// element keeps showing the last decoded frame meanwhile, so there is never a
// blank/loading state; scrubbing paints the newest requested frame as soon as
// the decoder can deliver it. requestVideoFrameCallback reports which frame
// is actually on screen so overlays stay in sync with the picture.

import { clamp } from "./util.js";

export class Player {
  constructor(video, onFrame) {
    this.video = video;
    this.onFrame = onFrame;
    this.fps = 30;
    this.t0 = 0;
    this.frameCount = 1;
    this.target = 0;
    this.inflight = null;
    this.seeking = false;
    this.shown = -1;
    this._fallback = null;
    this.onPresent = null; // (now, mediaTime) per presented frame, for the Puppeteer pass
    this.hasRvfc = "requestVideoFrameCallback" in HTMLVideoElement.prototype;
    video.addEventListener("seeked", () => this._seeked());
    if (this.hasRvfc) {
      const cb = (now, md) => {
        this.onPresent?.(now, md.mediaTime);
        this._report(this.timeToFrame(md.mediaTime));
        video.requestVideoFrameCallback(cb);
      };
      video.requestVideoFrameCallback(cb);
    } else {
      video.addEventListener("timeupdate", () => this.playing && this._report(this.timeToFrame(video.currentTime)));
    }
  }

  setMeta(meta) {
    this.fps = meta.fps;
    this.t0 = meta.t0 || 0;
    this.frameCount = Math.max(1, meta.frameCount);
  }

  // Call before swapping the element's source; in-flight seeks never land.
  reset() {
    clearTimeout(this._fallback);
    this.seeking = false;
    this.inflight = null;
    this.shown = -1;
  }

  frameTime(i) {
    const t = this.t0 + (i + 0.5) / this.fps;
    const d = this.video.duration;
    return Number.isFinite(d) ? Math.min(t, Math.max(0, d - 0.0005)) : t;
  }

  timeToFrame(t) {
    return clamp(Math.round((t - this.t0) * this.fps), 0, this.frameCount - 1);
  }

  get playing() {
    return !this.video.paused && !this.video.ended;
  }

  seek(i) {
    this.target = clamp(Math.round(i), 0, this.frameCount - 1);
    if (this.playing) this.video.pause();
    if (!this.seeking) this._issue();
  }

  _issue() {
    this.seeking = true;
    this.inflight = this.target;
    this.video.currentTime = this.frameTime(this.inflight);
  }

  _seeked() {
    if (!this.seeking) return;
    this.seeking = false;
    const landed = this.inflight;
    // rVFC normally reports the new frame; fall back if it doesn't fire (e.g.
    // the frame was already on screen).
    clearTimeout(this._fallback);
    this._fallback = setTimeout(() => {
      if (this.shown !== landed && !this.seeking) this._report(landed);
    }, this.hasRvfc ? 120 : 0);
    if (this.target !== this.inflight) this._issue();
  }

  _report(i) {
    this.shown = i;
    this.onFrame(i);
  }

  play() {
    if (this.target >= this.frameCount - 1) this.seek(0);
    this.video.play().catch(() => {});
  }

  pause() {
    if (!this.playing) return;
    this.video.pause();
    this.seek(this.shown >= 0 ? this.shown : this.target);
  }

  toggle() {
    if (this.playing) this.pause();
    else this.play();
  }
}
