// Puppeteer pass (P): record a subject's bounds by following it with the mouse
// while the video plays slowed down. Jiggling the mouse makes the box bigger.
// Each finished pass is stored independently (project.boundsPasses) and the
// effective bounds are their composite, so a rough pass stays as fallback under
// finer ones and comparable takes can be averaged. Recording lives here; the
// maths is in bounds.js (synthesize) and bounds-stack.js (composite).
// See editor/PLAN.md §6.

import { PUPPETEER_DEFAULTS, synthesize } from "./bounds.js";
import { clamp, hexToRgba, toast } from "./util.js";

const SETTINGS_KEY = "cotrack.puppeteer";
const COUNTDOWN_MS = 500;
export const SPEEDS = [0.25, 0.5, 0.7, 1];

// Capture presets: a slower pass with a smaller minimum box and less padding
// can actually get tight around a small subject.
export const PRESETS = {
  rough: { label: "Rough (1×)", rate: 1, minHalf: 20, pad: 16, sigmaCenter: 0.12, sigmaSize: 0.25, smoothPos: 0.08, smoothSize: 0.2 },
  fine: { label: "Fine (0.5×)", rate: 0.5, minHalf: 10, pad: 8, sigmaCenter: 0.1, sigmaSize: 0.18, smoothPos: 0.06, smoothSize: 0.15 },
  extra: { label: "Extra fine (0.25×)", rate: 0.25, minHalf: 6, pad: 5, sigmaCenter: 0.08, sigmaSize: 0.14, smoothPos: 0.05, smoothSize: 0.12 },
};
const PRESET_KEYS = ["rate", "minHalf", "pad", "sigmaCenter", "sigmaSize", "smoothPos", "smoothSize"];

export function presetValues(preset) {
  const p = PRESETS[preset] || PRESETS.fine;
  const out = {};
  for (const k of PRESET_KEYS) out[k] = p[k];
  return out;
}

export function puppeteerSettings() {
  let saved = {};
  try {
    saved = JSON.parse(localStorage.getItem(SETTINGS_KEY) || "{}");
  } catch {}
  const preset = PRESETS[saved.preset] ? saved.preset : "fine";
  return {
    preset,
    rate: SPEEDS.includes(saved.rate) ? saved.rate : PRESETS[preset].rate,
    lag: clamp(Number(saved.lag ?? PUPPETEER_DEFAULTS.lag), 0, 1),
    gain: clamp(Number(saved.gain ?? PUPPETEER_DEFAULTS.gain), 0.25, 4),
    minHalf: clamp(Number(saved.minHalf ?? PRESETS[preset].minHalf), 2, 400),
    pad: clamp(Number(saved.pad ?? PRESETS[preset].pad), 0, 200),
  };
}

export function savePuppeteerSettings(patch) {
  localStorage.setItem(SETTINGS_KEY, JSON.stringify({ ...puppeteerSettings(), ...patch }));
}

// Re-synthesize a stored pass from its original pointer samples (settings or
// range changed). Returns a Float32Array of [cx, cy, w, h] per frame from a.
export function regeneratePass(app, pass) {
  const samples = pass.samples.slice().sort((p, q) => p.u - q.u);
  const settings = { ...PUPPETEER_DEFAULTS, ...pass.settings };
  return synthesize(samples, {
    fps: app.meta.fps, a: pass.a, b: pass.b, width: app.meta.width, height: app.meta.height, ...settings,
  });
}

export class PuppeteerMode {
  constructor(viewer) {
    this.v = viewer;
    this.app = viewer.app;
    this.name = "puppeteer";
    this.state = "idle";
    this.mouse = null; // last pointer position, canvas-local screen px
    this._onWinMove = (e) => this.onWindowMove(e);
    this._onWinDown = (e) => this.onWindowDown(e);
    this._onEnded = () => this.state === "recording" && this.finish("end of video");
  }

  get subject() {
    return this.app.project.subject(this.app.selSubject);
  }

  enter() {
    this.app.player.pause();
    this.state = "armed";
    this.samples = [];
    window.addEventListener("pointermove", this._onWinMove, true);
  }

  exit() {
    if (this.state === "countdown" || this.state === "recording") this.cleanupPlayback();
    window.removeEventListener("pointermove", this._onWinMove, true);
    window.removeEventListener("pointerdown", this._onWinDown, true);
    this.state = "idle";
    cancelAnimationFrame(this._raf);
  }

  local(e) {
    const r = this.v.canvas.getBoundingClientRect();
    return [e.clientX - r.left, e.clientY - r.top];
  }

  // ---- input -------------------------------------------------------------------------------

  onWindowMove(e) {
    this.mouse = this.local(e);
    if (this.state !== "recording") {
      if (this.state === "armed") this.v.requestDraw();
      return;
    }
    // Pointer samples between frames, mapped onto video time with the last
    // presented frame's (now, mediaTime) pair.
    const events = e.getCoalescedEvents ? e.getCoalescedEvents() : [e];
    for (const ev of events.length ? events : [e]) this.addPointerSample(ev);
  }

  addPointerSample(ev) {
    const ref = this.ref;
    if (!ref) return;
    const [sx, sy] = this.local(ev);
    const [x, y] = this.v.toSource(sx, sy);
    const u = ref.u + ((ev.timeStamp - ref.now) / 1000) * this.rate * this.fps;
    if (u >= this.startFrame) this.samples.push({ u, x, y });
  }

  onDown(e, sx, sy) {
    if (this.state === "armed") this.startCountdown(sx, sy);
    return false;
  }

  // Any click during the pass stops it, wherever it lands.
  onWindowDown(e) {
    if (this.state !== "recording" && this.state !== "countdown") return;
    e.preventDefault();
    e.stopPropagation();
    if (this.state === "recording") this.finish("click");
    else this.cancel();
  }

  onMove() {
    return true;
  }

  onUp() {
    return true;
  }

  onWheel() {
    return this.state === "recording" || this.state === "countdown";
  }

  onKey(e) {
    const stopKey = e.key === "Escape" || e.key === " " || ((e.key === "p" || e.key === "P") && !e.ctrlKey && !e.metaKey);
    if (this.state === "recording") {
      if (stopKey) this.finish("key");
      return true; // nothing else while recording (no seeking)
    }
    if (this.state === "countdown") {
      if (stopKey) this.cancel();
      return true;
    }
    if (stopKey) {
      this.v.setMode(null);
      return true;
    }
    return false;
  }

  // ---- recording ----------------------------------------------------------------------------

  startCountdown(sx, sy) {
    const app = this.app;
    const s = this.subject;
    if (!s || !app.meta) return;
    const f = this.v.frame();
    if (f >= app.meta.frameCount - 2) {
      toast("The pass starts at the current frame — move the cursor before the end of the video.", "error");
      return;
    }
    const settings = puppeteerSettings();
    const preset = presetValues(settings.preset);
    this.sid = s.id;
    this.rate = settings.rate;
    this.lag = settings.lag;
    this.gain = settings.gain;
    this.capture = { ...preset, rate: settings.rate, lag: settings.lag, gain: settings.gain,
      minHalf: settings.minHalf, pad: settings.pad };
    this.fps = app.meta.fps;
    this.lagFrames = this.lag * this.rate * this.fps;
    this.startFrame = f;
    this.mouse = [sx, sy];
    this.samples = [];
    this.ref = null;
    this.state = "countdown";
    this.t0 = performance.now();
    window.addEventListener("pointerdown", this._onWinDown, true);
    const tick = () => {
      if (this.state === "countdown" && performance.now() - this.t0 >= COUNTDOWN_MS) this.startRecording();
      if (this.state === "countdown" || this.state === "recording") {
        this.v.requestDraw();
        this._raf = requestAnimationFrame(tick);
      }
    };
    this._raf = requestAnimationFrame(tick);
  }

  startRecording() {
    const { app } = this;
    const video = app.video;
    this.state = "recording";
    const [x, y] = this.v.toSource(...this.mouse);
    this.samples.push({ u: this.startFrame, x, y });
    app.player.onPresent = (now, mediaTime) => this.onPresent(now, mediaTime);
    video.addEventListener("ended", this._onEnded);
    video.playbackRate = this.rate;
    app.player.play();
  }

  // One sample per presented frame: the last pointer position, so a still
  // mouse still produces data.
  onPresent(now, mediaTime) {
    if (this.state !== "recording") return;
    const u = (mediaTime - this.app.player.t0) * this.fps;
    this.ref = { now, u };
    if (u < this.startFrame - 0.5) return;
    const [x, y] = this.v.toSource(...this.mouse);
    this.samples.push({ u, x, y, frame: true });
    if (u >= this.app.meta.frameCount - 1.5) this.finish("end of video");
  }

  cleanupPlayback() {
    const { app } = this;
    app.player.onPresent = null;
    app.video.removeEventListener("ended", this._onEnded);
    window.removeEventListener("pointerdown", this._onWinDown, true);
    app.player.pause();
    app.video.playbackRate = 1;
    cancelAnimationFrame(this._raf);
  }

  cancel() {
    this.cleanupPlayback();
    this.state = "armed";
    toast("Puppeteer pass cancelled.");
    this.v.requestDraw();
  }

  finish(reason) {
    if (this.state !== "recording") return;
    this.cleanupPlayback();
    const { app } = this;
    const N = app.meta.frameCount;
    const a = this.startFrame;
    const last = this.samples.reduce((m, p) => (p.frame ? Math.max(m, p.u) : m), a);
    const b = clamp(Math.floor(last), a, N - 1);
    this.state = "idle";
    this.v.setMode(null);
    if (b - a < 2) {
      toast("The pass was too short to record bounds — press P and follow the subject a little longer.", "error");
      return;
    }
    const samples = this.samples.slice().sort((p, q) => p.u - q.u);
    const settings = this.capture;
    const boxes = synthesize(samples, {
      fps: this.fps, a, b, width: app.meta.width, height: app.meta.height, ...settings,
    });
    const s = app.project.subject(this.sid);
    const target = app.puppeteerTarget || { kind: "refine" };
    const levels = (s?.boundsPasses || []).map((p) => p.level ?? 0);
    const kind = target.kind === "take" ? "take" : "refine";
    const level = kind === "take" ? (target.level ?? Math.max(0, ...levels)) : (levels.length ? Math.max(...levels) + 1 : 0);
    const word = (PRESETS[settings.preset]?.label || "Pass").replace(/ \(.*/, "");
    const pass = app.addBoundsPass(this.sid, {
      name: `${word} ${level}${kind === "take" ? " take" : ""}`, level, kind, a, b, samples, settings, boxes,
    });
    if (pass) {
      app.seek(a);
      toast(`${s?.name}: ${kind === "take" ? "take averaged into" : "pass recorded at"} level ${level} for frames ${a}–${b} (stopped by ${reason}). ` +
        "B adjusts the manual layer; the pass list below can disable, retune or delete it.");
    }
  }

  // ---- drawing ------------------------------------------------------------------------------

  draw(ctx) {
    const s = this.subject;
    if (!s || !this.mouse) return;
    const [mx, my] = this.mouse;
    ctx.save();
    if (this.state === "armed") {
      ctx.strokeStyle = hexToRgba(s.color, 0.9);
      ctx.lineWidth = 1.5;
      ctx.setLineDash([4, 3]);
      ctx.beginPath();
      ctx.arc(mx, my, 18, 0, Math.PI * 2);
      ctx.stroke();
    } else if (this.state === "countdown") {
      const t = clamp(1 - (performance.now() - this.t0) / COUNTDOWN_MS, 0, 1);
      ctx.strokeStyle = s.color;
      ctx.lineWidth = 3;
      ctx.beginPath();
      ctx.arc(mx, my, 6 + 30 * t, 0, Math.PI * 2);
      ctx.stroke();
    } else if (this.state === "recording") {
      const box = this.liveBox();
      if (box) {
        const r = this.v.boundsRect(box);
        ctx.fillStyle = hexToRgba(s.color, 0.08);
        ctx.fillRect(r.x, r.y, r.w, r.h);
        this.v.drawBounds(ctx, s, box, true);
      }
      // The committed composite, faint, for comparison while following.
      const f = this.app.shown >= 0 ? this.app.shown : this.app.cursor;
      const existing = this.app.project.boundsAt(s, f);
      if (existing) this.v.drawBounds(ctx, s, existing, false);
      ctx.fillStyle = "#ef4444";
      ctx.beginPath();
      ctx.arc(mx, my, 2.5, 0, Math.PI * 2);
      ctx.fill();
      this.v.label(ctx, `● REC ${this.rate}×`, 12, 22, "rgba(127,29,29,.9)", "#fff");
    }
    ctx.restore();
  }

  // The box the pass is writing right now, live: the samples so far synthesised
  // around the frame the hand is on. The recording is shifted earlier by the
  // lag, so the box under the hand belongs to a slightly earlier frame — what
  // is drawn is what the pass will keep.
  liveBox() {
    const { app } = this;
    const N = app.meta.frameCount;
    const shown = app.shown >= 0 ? app.shown : app.cursor;
    const g = clamp(Math.round(shown - this.lagFrames), this.startFrame, N - 1);
    const o = { ...PUPPETEER_DEFAULTS, ...(this.capture || {}) };
    const W = Math.ceil(this.fps * o.rate * (o.after + 3 * o.sigmaSize)) + 2;
    const a = Math.max(this.startFrame, g - W);
    const b = Math.min(N - 1, g + W);
    if (b <= a || !this.samples.length) return null;
    const boxes = synthesize(this.samples, {
      fps: this.fps, a, b, width: app.meta.width, height: app.meta.height, ...o,
    });
    const i = clamp(g - a, 0, b - a);
    return boxes.subarray(4 * i, 4 * i + 4);
  }

  hint() {
    const s = this.subject;
    if (!s) return "Puppeteer: select a subject first.";
    const { rate, preset } = puppeteerSettings();
    const target = this.app.puppeteerTarget || { kind: "refine" };
    const mode = target.kind === "take" ? `averaging a take into level ${target.level}` : "recording a new refinement level";
    if (this.state === "armed") {
      return `Puppeteer for “${s.name}” (${mode}): click the subject, then follow it with the mouse as the video plays at ${rate}× — ` +
        `${PRESETS[preset]?.label || preset} preset · jiggle the mouse to make the box bigger · Esc, Space or a click stops · Esc now cancels.`;
    }
    if (this.state === "countdown") return "Get ready…";
    if (this.state === "recording") return "Follow the subject · jiggle = bigger box · Esc, Space or click to stop.";
    return "";
  }
}
