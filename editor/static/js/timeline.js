import { falloff } from "./bounds.js";
import { trackerLabel, VIS_THRESHOLD } from "./project.js";
import { clamp, fitCanvas, hexToRgba, stripePattern, timecode } from "./util.js";

const GUTTER = 150;
const RULER = 24;
const TRACK_ROW = 10;
const SUBJECT_H = 22;
const POINT_H = 16;
const SCROLL_H = 12;
const MANUAL = "#f59e0b";
const BG = "#14171d";

export class Timeline {
  constructor(app) {
    this.app = app;
    this.canvas = document.getElementById("timeline");
    this.lanes = document.createElement("canvas");
    this.v0 = 0;
    this.v1 = 100;
    this.scrollY = 0;
    this.w = 1;
    this.h = 1;
    this.dirty = true;
    this._raf = 0;
    this.hover = null;
    this.mode = null;
    this.endPreview = null;
    this.rowsCache = [];

    new ResizeObserver(() => this.invalidate()).observe(this.canvas);
    const c = this.canvas;
    c.addEventListener("pointerdown", (e) => this.onDown(e));
    c.addEventListener("pointermove", (e) => this.onMove(e));
    c.addEventListener("pointerup", (e) => this.onUp(e));
    c.addEventListener("pointercancel", (e) => this.onUp(e));
    c.addEventListener("pointerleave", () => {
      this.hover = null;
      this.requestDraw();
    });
    c.addEventListener("wheel", (e) => this.onWheel(e), { passive: false });
  }

  get N() {
    return this.app.meta ? Math.max(1, this.app.meta.frameCount) : 1;
  }

  get trackW() {
    return Math.max(1, this.w - GUTTER - 10);
  }

  get top() {
    return RULER + TRACK_ROW;
  }

  get bottom() {
    return this.h - SCROLL_H;
  }

  reset() {
    this.v0 = 0;
    this.v1 = this.N;
    this.scrollY = 0;
    this.invalidate();
  }

  invalidate() {
    this.dirty = true;
    this.requestDraw();
  }

  requestDraw() {
    if (!this._raf) {
      this._raf = requestAnimationFrame(() => {
        this._raf = 0;
        this.draw();
      });
    }
  }

  x(f) {
    return GUTTER + ((f - this.v0) / (this.v1 - this.v0)) * this.trackW;
  }

  f(x) {
    return this.v0 + ((x - GUTTER) / this.trackW) * (this.v1 - this.v0);
  }

  setView(v0, v1) {
    const N = this.N;
    const span = clamp(v1 - v0, Math.min(30, N), N);
    this.v0 = clamp(v0, 0, N - span);
    this.v1 = this.v0 + span;
    this.invalidate();
  }

  zoomAt(factor, x) {
    const fm = this.f(x);
    const t = (fm - this.v0) / (this.v1 - this.v0);
    const span = (this.v1 - this.v0) * factor;
    this.setView(fm - t * span, fm - t * span + span);
  }

  ensureVisible(f) {
    const span = this.v1 - this.v0;
    if (f < this.v0 || f + 1 > this.v1) {
      const v0 = f < this.v0 ? f - span * 0.1 : f + 1 - span * 0.9;
      this.setView(v0, v0 + span);
    }
  }

  rows() {
    const out = [];
    let y = 0;
    const { project } = this.app;
    for (const s of project.subjects) {
      out.push({ kind: "subject", s, y, h: SUBJECT_H });
      y += SUBJECT_H;
      for (const p of project.trackersOf(s.id)) {
        out.push({ kind: "tracker", s, p, y, h: POINT_H });
        y += POINT_H;
      }
    }
    this.contentH = y;
    const visH = this.bottom - this.top;
    this.scrollY = clamp(this.scrollY, 0, Math.max(0, y - visH));
    this.rowsCache = out;
    return out;
  }

  rowAt(y) {
    if (y < this.top || y >= this.bottom) return null;
    const cy = y - this.top + this.scrollY;
    return this.rowsCache.find((r) => cy >= r.y && cy < r.y + r.h) || null;
  }

  // ---- status keys per frame -----------------------------------------------------------

  subjectKey(s, f) {
    const st = this.app.project.subjectState(s, f, this.app.results);
    switch (st.status) {
      case "none":
        return null;
      case "unknown":
        return "unknown";
      case "lost":
        return st.lost ? "lost" : "drifted";
      case "partial":
        return "partial";
      default:
        return st.visible === st.n ? "vis" : st.visible === 0 ? "hid" : "mix";
    }
  }

  pointKey(p, f) {
    const { project, results } = this.app;
    const st = project.trackerState(p, f, results);
    if (!st) return null;
    if (st.lost) return "lost";
    if (st.drifted) return "drifted";
    const off = st.x != null && !project.isInside(st.x, st.y) ? "Off" : "";
    if (st.mode === "auto") return (st.vis >= VIS_THRESHOLD ? "vis" : "hid") + off;
    if (st.mode === "manual") return "manual" + off;
    return st.mode;
  }

  drawStatus(ctx, y, h, keyAt, styles) {
    const N = this.N;
    const tw = this.trackW;
    const fpp = (this.v1 - this.v0) / tw;
    if (fpp <= 1) {
      const fa = Math.max(0, Math.floor(this.v0));
      const fb = Math.min(N, Math.ceil(this.v1));
      for (let f = fa; f < fb; f++) {
        const k = keyAt(f);
        if (!k) continue;
        const x0 = Math.max(GUTTER, this.x(f));
        const x1 = Math.min(GUTTER + tw, this.x(f + 1));
        const gap = x1 - x0 > 6 ? 1 : 0;
        ctx.fillStyle = styles[k];
        ctx.fillRect(x0, y, Math.max(1, x1 - x0 - gap), h);
      }
      return;
    }
    let runKey = null;
    let runStart = 0;
    for (let c = 0; c <= tw; c++) {
      let k = null;
      if (c < tw) {
        const f = Math.floor(this.v0 + (c + 0.5) * fpp);
        if (f >= 0 && f < N) k = keyAt(f);
      }
      if (k !== runKey) {
        if (runKey) {
          ctx.fillStyle = styles[runKey];
          ctx.fillRect(GUTTER + runStart, y, c - runStart, h);
        }
        runKey = k;
        runStart = c;
      }
    }
  }

  // ---- rendering --------------------------------------------------------------------------

  renderLanes(dpr) {
    const { w, h } = this;
    const lanes = this.lanes;
    lanes.width = Math.round(w * dpr);
    lanes.height = Math.round(h * dpr);
    const ctx = lanes.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = BG;
    ctx.fillRect(0, 0, w, h);
    if (!this.app.ready) return;
    const { project } = this.app;
    const tw = this.trackW;

    this.renderRuler(ctx);

    // tracker coverage row
    ctx.fillStyle = "#0f1216";
    ctx.fillRect(GUTTER, RULER, tw, TRACK_ROW);
    ctx.fillStyle = "#6b7280";
    ctx.font = "10px system-ui";
    ctx.textBaseline = "middle";
    ctx.fillText("tracker", 10, RULER + TRACK_ROW / 2);
    const run = this.app.tracker.run;
    if (run && run.first != null) {
      const a = this.x(run.first);
      const b = this.x(this.app.tracker.status.frame ?? run.first);
      ctx.fillStyle = "rgba(34,211,238,.35)";
      ctx.fillRect(Math.max(GUTTER, a), RULER + 2, Math.min(GUTTER + tw, b) - Math.max(GUTTER, a), TRACK_ROW - 4);
    }

    const rows = this.rows();
    const pending = stripePattern(ctx, "#4b5563", "#2b313c");
    const blocked = stripePattern(ctx, "#9f1239", "#3b1d24");
    ctx.save();
    ctx.beginPath();
    ctx.rect(0, this.top, w, this.bottom - this.top);
    ctx.clip();
    for (const row of rows) {
      const y = this.top + row.y - this.scrollY;
      if (y + row.h < this.top || y > this.bottom) continue;
      const s = row.s;
      if (row.kind === "subject") {
        const sel = this.app.selSubject === s.id && this.app.selTracker == null;
        ctx.fillStyle = sel ? "#16262f" : this.app.selSubject === s.id ? "#141e25" : "#11141a";
        ctx.fillRect(0, y, w, row.h);
        ctx.fillStyle = s.color;
        ctx.fillRect(10, y + 6, 10, 10);
        ctx.fillStyle = s.hidden ? "#6b7280" : "#e6e8ec";
        ctx.font = "600 12px system-ui";
        ctx.fillText(this.clip(ctx, s.name, GUTTER - 36), 26, y + row.h / 2 + 1);
        const styles = {
          vis: hexToRgba(s.color, 0.9),
          mix: hexToRgba(s.color, 0.55),
          hid: hexToRgba(s.color, 0.25),
          partial: stripePattern(ctx, hexToRgba(s.color, 0.85), hexToRgba(s.color, 0.3)),
          unknown: pending,
          lost: "#6b2135",
          drifted: "#9a3412",
        };
        this.drawStatus(ctx, y + 4, row.h - 8, (f) => this.subjectKey(s, f), styles);
        const boundsExt = project.boundsExtent(s);
        if (boundsExt) {
          this.drawStatus(ctx, y + row.h - 3, 2, (f) => (project.boundsAt(s, f) ? "b" : null), { b: hexToRgba(s.color, 0.95) });
        }
        this.renderSubjectMarks(ctx, s, y, row.h);
      } else {
        const p = row.p;
        const sel = this.app.isSelected(p.id);
        ctx.fillStyle = sel ? (this.app.selTracker === p.id ? "#232c3a" : "#1b222d") : "#0e1116";
        ctx.fillRect(0, y, w, row.h);
        ctx.fillStyle = sel ? "#fff" : "#9aa3b2";
        ctx.font = `${sel ? 600 : 400} 11px system-ui`;
        ctx.fillText(trackerLabel(p), 30, y + row.h / 2 + 1);
        const styles = {
          vis: hexToRgba(s.color, 0.8),
          hid: hexToRgba(s.color, 0.28),
          visOff: stripePattern(ctx, hexToRgba(s.color, 0.8), hexToRgba(s.color, 0.3)),
          hidOff: stripePattern(ctx, hexToRgba(s.color, 0.35), hexToRgba(s.color, 0.1)),
          manual: hexToRgba(MANUAL, 0.85),
          manualOff: stripePattern(ctx, hexToRgba(MANUAL, 0.9), hexToRgba(MANUAL, 0.3)),
          pending,
          blocked,
          lost: "#9f1239",
          drifted: "#c2410c",
          nodata: "#3b1d24",
        };
        this.drawStatus(ctx, y + 4, row.h - 8, (f) => this.pointKey(p, f), styles);
        this.renderPointMarks(ctx, p, s, y, row.h);
      }
    }
    ctx.restore();

    ctx.fillStyle = "#262b35";
    ctx.fillRect(GUTTER - 1, RULER, 1, this.bottom - RULER);

    // horizontal scrollbar / overview
    const N = this.N;
    ctx.fillStyle = "#0f1216";
    ctx.fillRect(GUTTER, this.bottom + 2, tw, SCROLL_H - 4);
    const tx = GUTTER + (this.v0 / N) * tw;
    const tW = Math.max(8, ((this.v1 - this.v0) / N) * tw);
    ctx.fillStyle = "#3a4150";
    ctx.beginPath();
    ctx.roundRect(tx, this.bottom + 2, tW, SCROLL_H - 4, 3);
    ctx.fill();
    if (this.contentH > this.bottom - this.top) {
      ctx.fillStyle = "#6b7280";
      ctx.font = "10px system-ui";
      ctx.fillText("scroll names ↕", 10, this.bottom + SCROLL_H / 2);
    }
  }

  renderSubjectMarks(ctx, s, y, h) {
    const cy = y + h / 2;
    let drawn = 0;
    for (const k of s.offset) {
      if (k.f < this.v0 - 1 || k.f > this.v1 + 1) continue;
      if (++drawn > 400) break;
      const x = this.x(k.f + 0.5);
      const d = 4;
      ctx.beginPath();
      ctx.moveTo(x, cy - d);
      ctx.lineTo(x + d, cy);
      ctx.lineTo(x, cy + d);
      ctx.lineTo(x - d, cy);
      ctx.closePath();
      ctx.fillStyle = "#ffffff";
      ctx.fill();
      ctx.strokeStyle = s.color;
      ctx.lineWidth = 1.5;
      ctx.stroke();
    }
  }

  renderPointMarks(ctx, p, s, y, h) {
    const { project } = this.app;
    const cy = y + h / 2;
    const inView = (f) => f >= this.v0 - 1 && f <= this.v1 + 1;
    if (inView(p.start)) {
      ctx.fillStyle = "rgba(255,255,255,.7)";
      ctx.fillRect(this.x(p.start), y + 2, 1.5, h - 4);
    }
    const N = this.N;
    const end = project.end(p);
    const auto = p.autoEnd && p.autoEnd.f === end;
    const preview = this.endPreview?.pid === p.id ? this.endPreview.f : null;
    const shownEnd = preview ?? end;
    if (shownEnd < N && inView(shownEnd)) {
      const x = this.x(shownEnd);
      const focused = this.app.endMarkerFocus === p.id;
      ctx.fillStyle = auto ? "#fb923c" : "#f43f5e";
      ctx.fillRect(x - 1, y + 1, 2, h - 2);
      ctx.fillRect(x - 5, y + 1, 4, 2);
      ctx.fillRect(x - 5, y + h - 3, 4, 2);
      if (focused) {
        ctx.strokeStyle = "#fff";
        ctx.lineWidth = 1.5;
        ctx.strokeRect(x - 5.5, y + 0.5, 6, h - 1);
      }
      if (auto) {
        ctx.fillStyle = "#fb923c";
        ctx.font = "700 8px system-ui";
        ctx.textBaseline = "top";
        ctx.fillText("A", x + 3, y + 1);
      }
    }
    // Recorded bounds passes: one thin line per pass inside the subject row.
    const passes = (s.boundsPasses || []).filter((q) => q.enabled !== false && q.a != null);
    if (passes.length && this.app.selSubject === s.id) {
      let line = 0;
      for (const q of passes) {
        if (line >= 4) break;
        const a = this.x(q.a);
        const b = this.x(q.b);
        if (b < GUTTER || a > this.w) continue;
        ctx.fillStyle = hexToRgba(s.color, 0.25 + 0.2 * ((q.level ?? 0) % 3));
        ctx.fillRect(Math.max(GUTTER, a), y + 2 + line * 2, Math.min(this.w - 10, b) - Math.max(GUTTER, a), 1.5);
        line++;
      }
    }
    for (const r of p.manual) {
      const a = this.x(r.a);
      const b = this.x(r.b ?? project.end(p));
      if (b < GUTTER || a > this.w) continue;
      ctx.strokeStyle = MANUAL;
      ctx.lineWidth = 1;
      ctx.strokeRect(Math.max(GUTTER, a) + 0.5, y + 2.5, Math.min(this.w - 10, b) - Math.max(GUTTER, a) - 1, h - 5);
    }
    let drawn = 0;
    for (const k of p.keys) {
      if (!inView(k.f)) continue;
      if (++drawn > 400) break;
      const x = this.x(k.f + 0.5);
      const manual = project.manualRangeAt(p, k.f);
      const d = 4.5;
      ctx.beginPath();
      ctx.moveTo(x, cy - d);
      ctx.lineTo(x + d, cy);
      ctx.lineTo(x, cy + d);
      ctx.lineTo(x - d, cy);
      ctx.closePath();
      ctx.fillStyle = manual ? MANUAL : k.f === p.start ? "#ffffff" : s.color;
      ctx.fill();
      ctx.strokeStyle = "#05070a";
      ctx.lineWidth = 1;
      ctx.stroke();
    }
  }

  renderRuler(ctx) {
    const fps = this.app.meta.fps;
    const tw = this.trackW;
    ctx.fillStyle = "#101318";
    ctx.fillRect(0, 0, this.w, RULER);
    const pxPerFrame = tw / (this.v1 - this.v0);
    const cands = [1, 2, 5, 10];
    for (const sec of [0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200]) {
      cands.push(Math.max(1, Math.round(sec * fps)));
    }
    const major = cands.find((c) => c * pxPerFrame >= 90) || cands[cands.length - 1];
    let minor = major;
    for (const div of [10, 5, 4, 2]) {
      if (major % div === 0 && (major / div) * pxPerFrame >= 7) {
        minor = major / div;
        break;
      }
    }
    const wholeSeconds = major % Math.round(fps) === 0;
    ctx.font = "11px ui-monospace, Consolas, monospace";
    ctx.textBaseline = "top";
    ctx.save();
    ctx.beginPath();
    ctx.rect(GUTTER, 0, tw + 10, RULER);
    ctx.clip();
    const start = Math.max(0, Math.ceil(this.v0 / minor) * minor);
    for (let f = start; f <= this.v1; f += minor) {
      const x = Math.round(this.x(f)) + 0.5;
      const isMajor = f % major === 0;
      ctx.fillStyle = isMajor ? "#4b5563" : "#2b313c";
      ctx.fillRect(x, isMajor ? 10 : 16, 1, RULER - (isMajor ? 10 : 16));
      if (isMajor) {
        ctx.fillStyle = "#9aa3b2";
        let label = timecode(f, fps);
        if (wholeSeconds) label = label.slice(0, label.lastIndexOf(":"));
        ctx.fillText(label, x + 3, 2);
      }
    }
    ctx.restore();
    ctx.fillStyle = "#6b7280";
    ctx.font = "10px system-ui";
    ctx.textBaseline = "middle";
    const span = this.v1 - this.v0;
    ctx.fillText(`${Math.round(span).toLocaleString()} frames in view`, 10, RULER / 2);
  }

  clip(ctx, text, maxW) {
    if (ctx.measureText(text).width <= maxW) return text;
    let t = text;
    while (t.length > 1 && ctx.measureText(t + "…").width > maxW) t = t.slice(0, -1);
    return t + "…";
  }

  draw() {
    const { ctx, w, h, dpr } = fitCanvas(this.canvas);
    if (!w || !h) return;
    const sizeChanged = w !== this.w || h !== this.h;
    this.w = w;
    this.h = h;
    if (this.dirty || sizeChanged || this.lanes.width !== Math.round(w * dpr)) {
      this.renderLanes(dpr);
      this.dirty = false;
    }
    ctx.drawImage(this.lanes, 0, 0, w, h);
    if (!this.app.ready) return;
    const tw = this.trackW;
    const inTrack = (x) => x >= GUTTER && x <= GUTTER + tw;

    // tracker head
    const tr = this.app.tracker;
    if (tr.active && tr.status.frame != null) {
      const x = this.x(tr.status.frame);
      if (inTrack(x)) {
        ctx.fillStyle = "#22d3ee";
        ctx.fillRect(x - 1, RULER, 2, this.bottom - RULER);
      }
    }

    // Bounds mode: the falloff curve of a nudge over the ruler.
    const fo = this.app.viewer.mode.falloffOverlay?.();
    if (fo) {
      ctx.save();
      ctx.beginPath();
      ctx.rect(GUTTER, 0, tw, RULER);
      ctx.clip();
      ctx.beginPath();
      const steps = 80;
      for (let i = 0; i <= steps; i++) {
        const d = -fo.R + (2 * fo.R * i) / steps;
        const x = this.x(fo.f + 0.5 + d);
        const y = RULER - 2 - falloff(d, fo.R) * (RULER - 6);
        if (i) ctx.lineTo(x, y);
        else ctx.moveTo(x, y);
      }
      ctx.lineTo(this.x(fo.f + 0.5 + fo.R), RULER);
      ctx.lineTo(this.x(fo.f + 0.5 - fo.R), RULER);
      ctx.closePath();
      ctx.fillStyle = fo.active ? "rgba(34,211,238,.35)" : "rgba(34,211,238,.16)";
      ctx.fill();
      ctx.strokeStyle = "rgba(34,211,238,.9)";
      ctx.lineWidth = 1;
      ctx.stroke();
      ctx.restore();
    }

    // hover guide
    if (this.hover && inTrack(this.hover.x) && !this.mode) {
      ctx.fillStyle = "rgba(255,255,255,.18)";
      ctx.fillRect(Math.round(this.hover.x), RULER, 1, this.bottom - RULER);
    }

    // playhead
    const cur = this.app.cursor;
    const pxPerFrame = tw / (this.v1 - this.v0);
    const x0 = this.x(cur);
    if (pxPerFrame > 3 && inTrack(x0)) {
      ctx.fillStyle = "rgba(255,255,255,.08)";
      ctx.fillRect(x0, RULER, pxPerFrame, this.bottom - RULER);
    }
    const xc = this.x(cur + 0.5);
    if (inTrack(xc)) {
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(Math.round(xc) - 0.5, RULER - 4, 1.5, this.bottom - RULER + 4);
      const label = String(cur);
      ctx.font = "600 11px ui-monospace, Consolas, monospace";
      const lw = ctx.measureText(label).width + 10;
      const lx = clamp(xc - lw / 2, GUTTER, GUTTER + tw - lw);
      ctx.fillStyle = "#ffffff";
      ctx.beginPath();
      ctx.roundRect(lx, 2, lw, 16, 3);
      ctx.fill();
      ctx.fillStyle = "#05070a";
      ctx.textBaseline = "middle";
      ctx.fillText(label, lx + 5, 10.5);
    }

    if (this.hover && !this.mode) this.drawTooltip(ctx);
  }

  drawTooltip(ctx) {
    const { x, y } = this.hover;
    if (x < GUTTER || x > GUTTER + this.trackW) return;
    const f = clamp(Math.floor(this.f(x)), 0, this.N - 1);
    const row = this.rowAt(y);
    const { project, results } = this.app;
    let text = `frame ${f} · ${timecode(f, this.app.meta.fps)}`;
    if (row?.kind === "subject") {
      const st = project.subjectState(row.s, f, results);
      text += ` · ${row.s.name}: ${st.status}`;
       if (st.n) text += ` (${st.visible} visible, ${st.hidden} hidden${st.pending ? `, ${st.pending} pending` : ""}${st.lost ? `, ${st.lost} lost` : ""})`;
      if (st.drifted) text += ` · ${st.drifted} drifted`;
      const key = project.offsetKeyAt(row.s, f);
      if (key) text += ` · offset key ${key.dx.toFixed(1)}, ${key.dy.toFixed(1)}`;
      const b = project.boundsAt(row.s, f);
      if (b) text += ` · bounds ${Math.round(b[2])}×${Math.round(b[3])}`;
    } else if (row?.kind === "tracker") {
      const st = project.trackerState(row.p, f, results);
      const mode = st?.mode === "blocked" ? "off-frame seed (can't auto-track)" : st ? st.mode : "not present";
      text += ` · ${trackerLabel(row.p)}: ${mode}`;
       if (st?.mode === "auto") text += row.p.kind === "template" ? ` · score ${st.score.toFixed(3)} · look ${st.lookIndex >= 0 ? st.lookIndex + 1 : "old"}${st.lost ? " · not found" : ""}` : ` · visibility ${(st.vis * 100).toFixed(0)}%`;
      if (st?.drifted) text += " · drifted outside the bounds";
      if (st?.x != null && !project.isInside(st.x, st.y)) text += " · off-frame";
    }
    ctx.font = "11px system-ui";
    const w = ctx.measureText(text).width + 12;
    const tx = clamp(x + 12, GUTTER, this.w - w - 4);
    const ty = clamp(y - 26, RULER + 2, this.h - 22);
    ctx.fillStyle = "rgba(5,7,10,.92)";
    ctx.beginPath();
    ctx.roundRect(tx, ty, w, 18, 4);
    ctx.fill();
    ctx.fillStyle = "#e6e8ec";
    ctx.textBaseline = "middle";
    ctx.fillText(text, tx + 6, ty + 9.5);
  }

  // ---- interaction ---------------------------------------------------------------------------

  local(e) {
    const r = this.canvas.getBoundingClientRect();
    return [e.clientX - r.left, e.clientY - r.top];
  }

  seekFromX(x) {
    this.app.seek(clamp(Math.floor(this.f(clamp(x, GUTTER, GUTTER + this.trackW - 0.01))), 0, this.N - 1));
  }

  keyAt(row, x) {
    if (row?.kind !== "tracker") return null;
    let best = null;
    let bd = 6;
    for (const k of row.p.keys) {
      const d = Math.abs(this.x(k.f + 0.5) - x);
      if (d < bd) {
        bd = d;
        best = k;
      }
    }
    return best;
  }

  // The tracker's end marker near x, or null. Auto ends are draggable too:
  // moving one converts it into a user boundary.
  endAt(row, x) {
    if (row?.kind !== "tracker") return null;
    const p = row.p;
    const end = this.app.project.end(p);
    if (end >= this.N) return null;
    return Math.abs(this.x(end) - x) <= 6 ? end : null;
  }

  onDown(e) {
    if (!this.app.ready || e.button !== 0) return;
    const [x, y] = this.local(e);
    const app = this.app;
    if (y >= this.bottom) {
      const N = this.N;
      const tx = GUTTER + (this.v0 / N) * this.trackW;
      const tW = Math.max(8, ((this.v1 - this.v0) / N) * this.trackW);
      if (x >= tx && x <= tx + tW) this.mode = { kind: "thumb", dx: x - tx };
      else {
        const span = this.v1 - this.v0;
        const fc = ((x - GUTTER) / this.trackW) * N;
        this.setView(fc - span / 2, fc + span / 2);
        this.mode = { kind: "thumb", dx: tW / 2 };
      }
      this.canvas.setPointerCapture(e.pointerId);
      return;
    }
    const row = this.rowAt(y);
    const additive = e.ctrlKey || e.metaKey;
    if (row?.kind === "tracker" && additive) {
      app.select({ tracker: row.p.id, toggle: true });
      return;
    }
    if (x < GUTTER) {
      if (row?.kind === "subject") app.select({ subject: row.s.id, tracker: null });
      else if (row?.kind === "tracker") app.select({ subject: row.s.id, tracker: row.p.id });
      return;
    }
    const key = this.keyAt(row, x);
    if (key) {
      app.select({ subject: row.s.id, tracker: row.kind === "tracker" ? row.p.id : null });
      app.seek(key.f);
      return;
    }
    const end = this.endAt(row, x);
    if (end != null) {
      app.select({ subject: row.s.id, tracker: row.p.id, endFocus: true });
      app.endMarkerFocus = row.p.id;
      this.mode = { kind: "end", pid: row.p.id };
      this.invalidate();
      this.canvas.setPointerCapture(e.pointerId);
      return;
    }
    if (row?.kind === "tracker") app.select({ subject: row.s.id, tracker: row.p.id });
    else if (row?.kind === "subject") app.select({ subject: row.s.id, tracker: null });
    this.mode = { kind: "scrub" };
    app.player.pause();
    this.seekFromX(x);
    this.canvas.setPointerCapture(e.pointerId);
  }

  onMove(e) {
    const [x, y] = this.local(e);
    this.hover = { x, y };
    if (this.mode?.kind === "scrub") this.seekFromX(x);
    else if (this.mode?.kind === "end") {
      const p = this.app.project.tracker(this.mode.pid);
      if (p) {
        this.endPreview = { pid: p.id, f: clamp(Math.round(this.f(clamp(x, GUTTER, GUTTER + this.trackW - 0.01))), p.start + 1, this.N) };
        this.invalidate();
      }
    } else if (this.mode?.kind === "thumb") {
      const span = this.v1 - this.v0;
      const v0 = ((x - this.mode.dx - GUTTER) / this.trackW) * this.N;
      this.setView(v0, v0 + span);
    }
    this.requestDraw();
  }

  onUp(e) {
    const mode = this.mode;
    this.mode = null;
    if (mode?.kind === "end" && this.endPreview) {
      const { pid, f } = this.endPreview;
      const p = this.app.project.tracker(pid);
      this.endPreview = null;
      if (p && f !== this.app.project.end(p)) this.app.setTrackerEnd(pid, f);
      this.invalidate();
    }
    if (this.canvas.hasPointerCapture?.(e.pointerId)) this.canvas.releasePointerCapture(e.pointerId);
  }

  onWheel(e) {
    e.preventDefault();
    if (!this.app.ready) return;
    const [x] = this.local(e);
    if (x < GUTTER) {
      this.scrollY += e.deltaY;
      this.invalidate();
      return;
    }
    const span = this.v1 - this.v0;
    if (e.shiftKey || Math.abs(e.deltaX) > Math.abs(e.deltaY)) {
      const d = (e.shiftKey ? e.deltaY || e.deltaX : e.deltaX) * (span / this.trackW);
      this.setView(this.v0 + d, this.v1 + d);
    } else {
      this.zoomAt(Math.exp(e.deltaY * 0.0015), x);
    }
  }
}
