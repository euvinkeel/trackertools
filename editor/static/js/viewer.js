import { BoundsMode, NormalMode } from "./modes.js";
import { FinetuneMode } from "./finetune.js";
import { PuppeteerMode } from "./puppeteer.js";
import { trackerLabel, VIS_THRESHOLD } from "./project.js";
import { clamp, fitCanvas, hexToRgba } from "./util.js";

const TRACKER_HIT = 10;
const TRACKER_HIT_TIGHT = 6;
const SUBJECT_RING = [6, 15];
const TRAIL_FRAMES = 24;
const PATH_FRAMES = 120;
const MANUAL = "#f59e0b";
const DRIFT = "#fb923c";

export class Viewer {
  constructor(app) {
    this.app = app;
    this.el = document.getElementById("viewer");
    this.stage = document.getElementById("stage");
    this.canvas = document.getElementById("overlay");
    this.hintEl = document.getElementById("viewer-hint");
    this.zoomEl = document.getElementById("viewer-zoom");
    this.W = 16;
    this.H = 9;
    this.s = 1;
    this.ox = 0;
    this.oy = 0;
    this.cw = 1;
    this.ch = 1;
    this.fitted = true;
    this.pan = null;
    this.hover = null;
    this.spaceDown = false;
    this._raf = 0;
    this.normal = new NormalMode(this);
    this.boundsMode = new BoundsMode(this);
    this.puppeteer = new PuppeteerMode(this);
    this.finetune = new FinetuneMode(this);
    this.mode = this.normal;

    new ResizeObserver(() => (this.fitted ? this.fit() : this.requestDraw())).observe(this.el);
    const c = this.canvas;
    c.addEventListener("pointerdown", (e) => this.onDown(e));
    c.addEventListener("pointermove", (e) => this.onMove(e));
    c.addEventListener("pointerup", (e) => this.onUp(e));
    c.addEventListener("pointercancel", (e) => this.onUp(e));
    c.addEventListener("dblclick", (e) => this.onDouble(e));
    c.addEventListener("contextmenu", (e) => e.preventDefault());
    c.addEventListener("wheel", (e) => this.onWheel(e), { passive: false });
    c.addEventListener("pointerleave", () => {
      if (this.hover != null) {
        this.hover = null;
        this.requestDraw();
      }
    });
  }

  setMode(mode) {
    if (this.mode === mode) return;
    this.mode.exit();
    this.mode = mode || this.normal;
    this.mode.enter();
    this.canvas.dataset.mode = this.mode.name;
    this.requestDraw();
    this.app.onModeChanged?.();
  }

  // ---- view transform ------------------------------------------------------------

  setSize(W, H) {
    this.W = W;
    this.H = H;
    this.stage.style.width = `${W}px`;
    this.stage.style.height = `${H}px`;
    this.fit();
  }

  fitScale() {
    return Math.min(this.el.clientWidth / this.W, this.el.clientHeight / this.H) * 0.97;
  }

  fit() {
    const s = this.fitScale();
    this.setView(s, (this.el.clientWidth - this.W * s) / 2, (this.el.clientHeight - this.H * s) / 2);
    this.fitted = true;
  }

  setView(s, ox, oy) {
    this.s = s;
    this.ox = ox;
    this.oy = oy;
    this.stage.style.transform = `translate(${ox}px, ${oy}px) scale(${s})`;
    this.zoomEl.textContent = `${Math.round(s * 100)}%`;
    this.requestDraw();
  }

  zoomAt(factor, mx, my) {
    // Zooming far out leaves room to place and animate trackers off-frame.
    const s2 = clamp(this.s * factor, this.fitScale() * 0.08, 40);
    const ux = (mx - this.ox) / this.s;
    const uy = (my - this.oy) / this.s;
    this.setView(s2, mx - ux * s2, my - uy * s2);
    this.fitted = false;
  }

  toScreen(x, y) {
    return [this.ox + x * this.s, this.oy + y * this.s];
  }

  toSource(sx, sy) {
    return [(sx - this.ox) / this.s, (sy - this.oy) / this.s];
  }

  frame() {
    return this.app.shown >= 0 ? this.app.shown : this.app.cursor;
  }

  // Last pointer position in canvas pixels (for modes that need it, e.g. the
  // finetune loupe).
  hoverPoint() {
    return this._hoverPoint ?? null;
  }

  // ---- hit testing -------------------------------------------------------------------

  local(e) {
    const r = this.canvas.getBoundingClientRect();
    return [e.clientX - r.left, e.clientY - r.top];
  }

  nearestTracker(sx, sy, radius) {
    const { project, results } = this.app;
    const f = this.frame();
    let best = null;
    let bestD = radius;
    for (const s of project.subjects) {
      if (s.hidden) continue;
      for (const p of project.trackersOf(s.id)) {
        const st = project.trackerState(p, f, results);
        if (!st || st.x == null) continue;
        const [px, py] = this.toScreen(st.x, st.y);
        const d = Math.hypot(px - sx, py - sy) - (p.id === this.app.selTracker ? 3 : 0);
        if (d < bestD) {
          bestD = d;
          best = p;
        }
      }
    }
    return best;
  }

  // Subject markers are grabbed by their ring/arms, leaving the center free
  // for placing points.
  subjectRingAt(sx, sy) {
    const { project, results } = this.app;
    const f = this.frame();
    for (const s of project.subjects) {
      if (s.hidden) continue;
      const st = project.subjectState(s, f, results);
      if (st.x == null) continue;
      const [px, py] = this.toScreen(st.x, st.y);
      const d = Math.hypot(px - sx, py - sy);
      if (d >= SUBJECT_RING[0] && d <= SUBJECT_RING[1]) return s;
    }
    return null;
  }

  hitTest(sx, sy) {
    const tight = this.nearestTracker(sx, sy, TRACKER_HIT_TIGHT);
    if (tight) return { kind: "tracker", p: tight };
    const p = this.nearestTracker(sx, sy, TRACKER_HIT);
    const s = this.subjectRingAt(sx, sy);
    // The ring only wins drags; a click on it still selects the nearby tracker (`near`) or adds a point.
    if (s) return { kind: "subject", s, near: p };
    return p ? { kind: "tracker", p } : null;
  }

  trackersIn(rect) {
    const { project, results } = this.app;
    const f = this.frame();
    const ids = [];
    for (const s of project.subjects) {
      if (s.hidden) continue;
      for (const p of project.trackersOf(s.id)) {
        const st = project.trackerState(p, f, results);
        if (st?.x != null && st.x >= rect.x0 && st.x <= rect.x1 && st.y >= rect.y0 && st.y <= rect.y1) ids.push(p.id);
      }
    }
    return ids;
  }

  // ---- input -------------------------------------------------------------------------

  onDown(e) {
    const [sx, sy] = this.local(e);
    if (e.button === 1 || e.button === 2 || (e.button === 0 && this.spaceDown)) {
      e.preventDefault();
      this.pan = { sx, sy, ox: this.ox, oy: this.oy };
      this.canvas.setPointerCapture(e.pointerId);
      this.canvas.classList.add("panning");
      return;
    }
    if (e.button !== 0 || !this.app.ready) return;
    this.app.player.pause();
    if (this.mode.onDown(e, sx, sy)) this.canvas.setPointerCapture(e.pointerId);
  }

  onMove(e) {
    const [sx, sy] = this.local(e);
    this._hoverPoint = [sx, sy];
    if (this.pan) {
      this.setView(this.s, this.pan.ox + sx - this.pan.sx, this.pan.oy + sy - this.pan.sy);
      this.fitted = false;
      return;
    }
    if (!this.app.ready) return;
    if (this.mode.onMove(e, sx, sy)) return;
    const hit = this.hitTest(sx, sy);
    this.canvas.classList.toggle("on-point", hit?.kind === "tracker");
    this.canvas.classList.toggle("on-subject", hit?.kind === "subject");
    const id = hit ? `${hit.kind}:${hit.kind === "tracker" ? hit.p.id : hit.s.id}` : null;
    if (id !== this.hover) {
      this.hover = id;
      this.requestDraw();
    }
  }

  onUp(e) {
    if (this.pan) {
      this.pan = null;
      this.canvas.classList.remove("panning");
    } else if (this.app.ready) {
      const [sx, sy] = this.local(e);
      this.mode.onUp(e, sx, sy);
    }
    if (this.canvas.hasPointerCapture?.(e.pointerId)) this.canvas.releasePointerCapture(e.pointerId);
  }

  onDouble(e) {
    if (!this.app.ready || this.mode !== this.normal) return;
    const [sx, sy] = this.local(e);
    const hit = this.hitTest(sx, sy);
    if (hit?.kind === "tracker") this.app.endTracker(hit.p.id, this.frame());
  }

  onWheel(e) {
    e.preventDefault();
    if (this.mode.onWheel?.(e)) return;
    const [sx, sy] = this.local(e);
    this.zoomAt(Math.exp(-e.deltaY * 0.0015), sx, sy);
  }

  // ---- drawing ---------------------------------------------------------------------------

  requestDraw() {
    if (!this._raf) {
      this._raf = requestAnimationFrame(() => {
        this._raf = 0;
        this.draw();
      });
    }
  }

  drawNow() {
    if (this._raf) cancelAnimationFrame(this._raf);
    this._raf = 0;
    this.draw();
  }

  draw() {
    const { ctx, w, h } = fitCanvas(this.canvas);
    ctx.clearRect(0, 0, w, h);
    const app = this.app;
    this.cw = w;
    this.ch = h;
    if (!app.ready) {
      this.setHint("");
      return;
    }
    const { project } = app;
    const f = this.frame();
    ctx.lineJoin = "round";
    ctx.lineCap = "round";

    // Frame boundary: positions outside it are allowed (manual animation).
    const [fx, fy] = this.toScreen(0, 0);
    ctx.strokeStyle = "rgba(255,255,255,.22)";
    ctx.lineWidth = 1;
    ctx.strokeRect(Math.round(fx) - 0.5, Math.round(fy) - 0.5, Math.round(this.W * this.s) + 1, Math.round(this.H * this.s) + 1);

    for (const s of project.subjects) {
      if (s.hidden) continue;
      const b = this.mode.previewBounds?.(s, f) ?? project.boundsAt(s, f);
      if (b) this.drawBounds(ctx, s, b, s.id === app.selSubject);
    }
    this.mode.drawUnder?.(ctx);
    const sel = app.selTracker != null ? project.tracker(app.selTracker) : null;
    if (sel && !project.subject(sel.subjectId)?.hidden) this.drawPath(ctx, sel, f);
    for (const s of project.subjects) {
      if (s.hidden) continue;
      this.drawTrail(ctx, s, f);
      for (const p of project.trackersOf(s.id)) this.drawTracker(ctx, p, s, f);
    }
    for (const s of project.subjects) if (!s.hidden) this.drawCenter(ctx, s, f);
    this.mode.draw(ctx);
    this.setHint(this.mode.hint(f) ?? "");
  }

  boundsRect(b) {
    const [x, y] = this.toScreen(b[0] - b[2] / 2, b[1] - b[3] / 2);
    return { x, y, w: b[2] * this.s, h: b[3] * this.s };
  }

  drawBounds(ctx, s, b, strong) {
    const r = this.boundsRect(b);
    ctx.save();
    ctx.strokeStyle = hexToRgba(s.color, strong ? 0.9 : 0.35);
    ctx.lineWidth = strong ? 1.5 : 1;
    if (!strong) ctx.setLineDash([5, 4]);
    ctx.strokeRect(r.x, r.y, r.w, r.h);
    if (strong) {
      // Corner brackets make the box read as a region, not a selection.
      const k = Math.min(14, r.w / 3, r.h / 3);
      ctx.lineWidth = 3;
      ctx.beginPath();
      for (const [cx, cy, dx, dy] of [[r.x, r.y, 1, 1], [r.x + r.w, r.y, -1, 1], [r.x, r.y + r.h, 1, -1], [r.x + r.w, r.y + r.h, -1, -1]]) {
        ctx.moveTo(cx + dx * k, cy);
        ctx.lineTo(cx, cy);
        ctx.lineTo(cx, cy + dy * k);
      }
      ctx.stroke();
    }
    ctx.restore();
  }

  offView(sx, sy) {
    return sx < -4 || sy < -4 || sx > this.cw + 4 || sy > this.ch + 4;
  }

  // Arrow on the viewer edge pointing at something outside the current view.
  drawEdgeMarker(ctx, sx, sy, color, size, label) {
    const m = size + 4;
    const cx = clamp(sx, m, this.cw - m);
    const cy = clamp(sy, m, this.ch - m);
    const a = Math.atan2(sy - cy, sx - cx);
    ctx.save();
    ctx.translate(cx, cy);
    ctx.rotate(a);
    ctx.beginPath();
    ctx.moveTo(size, 0);
    ctx.lineTo(-size * 0.7, -size * 0.7);
    ctx.lineTo(-size * 0.35, 0);
    ctx.lineTo(-size * 0.7, size * 0.7);
    ctx.closePath();
    ctx.fillStyle = color;
    ctx.fill();
    ctx.strokeStyle = "rgba(0,0,0,.8)";
    ctx.lineWidth = 1.5;
    ctx.stroke();
    ctx.restore();
    if (label) {
      const lx = clamp(cx - Math.cos(a) * 30, 8, this.cw - 90);
      const ly = clamp(cy - Math.sin(a) * 18, 14, this.ch - 8);
      this.label(ctx, label, lx, ly, "rgba(0,0,0,.7)", color);
    }
  }

  drawTracker(ctx, p, s, f) {
    const { project, results } = this.app;
    const st = project.trackerState(p, f, results);
    if (!st || st.x == null) return;
    let { x, y } = st;
    const drag = this.mode.trackerDrag;
    if (drag && drag.trackerId === p.id) ({ x, y } = drag);
    const [sx, sy] = this.toScreen(x, y);
    const primary = this.app.selTracker === p.id;
    const selected = this.app.isSelected(p.id);
    const r = selected ? 6.5 : 5;
    if (this.offView(sx, sy)) {
      this.drawEdgeMarker(ctx, sx, sy, s.color, selected ? 8 : 6, primary ? trackerLabel(p) : null);
      return;
    }

    ctx.save();
    const square = p.kind === "template" && st.mode !== "manual";
    const shape = () => {
      ctx.beginPath();
      if (square) ctx.rect(sx - r, sy - r, r * 2, r * 2);
      else ctx.arc(sx, sy, r, 0, Math.PI * 2);
    };
    if (st.mode === "blocked") {
      ctx.setLineDash([3, 3]);
      ctx.strokeStyle = "rgba(244,63,94,.9)";
      ctx.lineWidth = 1.5;
      ctx.beginPath();
      if (square) ctx.rect(sx - r - 1, sy - r - 1, (r + 1) * 2, (r + 1) * 2);
      else ctx.arc(sx, sy, r + 1, 0, Math.PI * 2);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.fillStyle = "#f43f5e";
      ctx.font = "700 10px system-ui";
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillText("!", sx, sy + 0.5);
    } else if (st.mode === "pending") {
      ctx.setLineDash([3, 3]);
      ctx.strokeStyle = "rgba(203,213,225,.75)";
      ctx.lineWidth = 1.5;
      ctx.beginPath();
      if (square) ctx.rect(sx - r - 1, sy - r - 1, (r + 1) * 2, (r + 1) * 2);
      else ctx.arc(sx, sy, r + 1, 0, Math.PI * 2);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.fillStyle = "rgba(203,213,225,.9)";
      ctx.font = "600 10px system-ui";
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillText("?", sx, sy + 0.5);
    } else if (st.mode === "manual") {
      const d = r + 1.5;
      ctx.beginPath();
      ctx.moveTo(sx, sy - d);
      ctx.lineTo(sx + d, sy);
      ctx.lineTo(sx, sy + d);
      ctx.lineTo(sx - d, sy);
      ctx.closePath();
      ctx.fillStyle = s.color;
      ctx.fill();
      ctx.strokeStyle = MANUAL;
      ctx.lineWidth = 2;
      ctx.stroke();
    } else if (st.drifted) {
      shape();
      ctx.fillStyle = "rgba(0,0,0,.45)";
      ctx.fill();
      ctx.setLineDash([2, 2]);
      ctx.strokeStyle = DRIFT;
      ctx.lineWidth = 2;
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.beginPath();
      ctx.moveTo(sx - 2.5, sy - 2.5);
      ctx.lineTo(sx + 2.5, sy + 2.5);
      ctx.moveTo(sx + 2.5, sy - 2.5);
      ctx.lineTo(sx - 2.5, sy + 2.5);
      ctx.lineWidth = 1.5;
      ctx.stroke();
    } else if (st.vis >= VIS_THRESHOLD) {
      shape();
      ctx.fillStyle = s.color;
      ctx.fill();
      ctx.strokeStyle = "rgba(255,255,255,.95)";
      ctx.lineWidth = 1.5;
      ctx.stroke();
    } else {
      shape();
      ctx.fillStyle = "rgba(0,0,0,.35)";
      ctx.fill();
      ctx.setLineDash([3, 2]);
      ctx.strokeStyle = s.color;
      ctx.lineWidth = 2;
      ctx.stroke();
      ctx.setLineDash([]);
    }
    if (project.isKey(p, f) && st.mode !== "pending" && st.mode !== "blocked") {
      ctx.fillStyle = "#fff";
      ctx.fillRect(sx - 1.5, sy - 1.5, 3, 3);
    }
    if (selected) {
      if (square && p.looks?.length && st.mode === "auto") {
        const look = this.app.project.lookBySlot(p, st.look ?? 0) || p.looks[0];
        ctx.strokeStyle = hexToRgba(s.color, 0.85);
        ctx.lineWidth = 1;
        ctx.setLineDash([4, 3]);
        ctx.strokeRect(sx - look.hx * this.s, sy - look.hy * this.s, look.w * this.s, look.h * this.s);
        ctx.setLineDash([]);
      }
      ctx.beginPath();
      ctx.arc(sx, sy, r + 4.5, 0, Math.PI * 2);
      ctx.strokeStyle = primary ? "#fff" : "rgba(255,255,255,.7)";
      ctx.lineWidth = primary ? 1.75 : 1.25;
      if (!primary) ctx.setLineDash([3, 2]);
      ctx.stroke();
      ctx.setLineDash([]);
      if (primary || this.app.selTrackers.size <= 4) {
        this.label(ctx, trackerLabel(p), sx + r + 8, sy - r - 6, "rgba(0,0,0,.65)", "#fff");
      }
    } else if (this.hover === `tracker:${p.id}`) {
      ctx.beginPath();
      ctx.arc(sx, sy, r + 3.5, 0, Math.PI * 2);
      ctx.strokeStyle = hexToRgba(s.color, 0.7);
      ctx.lineWidth = 1.5;
      ctx.stroke();
    }
    ctx.restore();
  }

  // Subject marker at its final position. With an offset, a ghost marks the
  // trackers have pushed it to and a dashed line joins them.
  drawCenter(ctx, s, f) {
    const { project, results } = this.app;
    const st = project.subjectState(s, f, results);
    if (st.x == null) return;
    let fx = st.x;
    let fy = st.y;
    const drag = this.mode.offsetDrag;
    const dragging = drag?.sid === s.id;
    if (dragging) ({ x: fx, y: fy } = drag);
    else {
      const fp = this.mode.finePreview?.(s, f);
      if (fp) {
        fx += fp[0];
        fy += fp[1];
      }
    }
    const [sx, sy] = this.toScreen(fx, fy);
    const selected = this.app.selSubject === s.id;
    const hovered = this.hover === `subject:${s.id}`;
    const shifted = Math.hypot(fx - st.rawX, fy - st.rawY) > 0.01;

    if (shifted && (selected || dragging)) {
      const [gx, gy] = this.toScreen(st.rawX, st.rawY);
      ctx.save();
      ctx.setLineDash([4, 4]);
      ctx.strokeStyle = hexToRgba(s.color, 0.8);
      ctx.lineWidth = 1.25;
      ctx.beginPath();
      ctx.moveTo(gx, gy);
      ctx.lineTo(sx, sy);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.beginPath();
      ctx.arc(gx, gy, 4, 0, Math.PI * 2);
      ctx.strokeStyle = "rgba(0,0,0,.7)";
      ctx.lineWidth = 3;
      ctx.stroke();
      ctx.strokeStyle = hexToRgba(s.color, 0.9);
      ctx.lineWidth = 1.5;
      ctx.stroke();
      ctx.restore();
    }

    if (this.offView(sx, sy)) {
      this.drawEdgeMarker(ctx, sx, sy, s.color, 10, s.name);
      return;
    }
    ctx.save();
    if (st.status === "partial") ctx.setLineDash([3, 3]);
    const ring = hovered || dragging ? 9.5 : 8;
    for (const [color, width] of [["rgba(0,0,0,.7)", 4], [s.color, hovered || dragging ? 2.75 : 2]]) {
      ctx.strokeStyle = color;
      ctx.lineWidth = width;
      ctx.beginPath();
      ctx.moveTo(sx - 13, sy);
      ctx.lineTo(sx - 5, sy);
      ctx.moveTo(sx + 5, sy);
      ctx.lineTo(sx + 13, sy);
      ctx.moveTo(sx, sy - 13);
      ctx.lineTo(sx, sy - 5);
      ctx.moveTo(sx, sy + 5);
      ctx.lineTo(sx, sy + 13);
      ctx.stroke();
      ctx.beginPath();
      if (shifted) {
        // Offset subjects get a diamond so the final position reads differently.
        ctx.moveTo(sx, sy - ring);
        ctx.lineTo(sx + ring, sy);
        ctx.lineTo(sx, sy + ring);
        ctx.lineTo(sx - ring, sy);
        ctx.closePath();
      } else ctx.arc(sx, sy, ring, 0, Math.PI * 2);
      ctx.stroke();
    }
    ctx.setLineDash([]);
    let text = s.name;
    if (st.n > 1 || st.hidden || st.pending) {
      text += `  ${st.visible}/${st.n}`;
      if (st.pending) text += ` +${st.pending}?`;
    }
    if (st.lost) text += ` ${st.lost} lost`;
    this.label(ctx, text, sx + 12, sy + 14, selected ? hexToRgba(s.color, 0.9) : "rgba(0,0,0,.7)", selected ? "#05070a" : s.color);
    ctx.restore();
  }

  drawTrail(ctx, s, f) {
    const { project, results } = this.app;
    if (!project.trackersOf(s.id).length) return;
    let prev = null;
    ctx.save();
    ctx.lineWidth = 2;
    for (let t = f - TRAIL_FRAMES; t <= f; t++) {
      if (t < 0) continue;
      const st = project.subjectState(s, t, results);
      if (st.x == null) {
        prev = null;
        continue;
      }
      const pt = this.toScreen(st.x, st.y);
      if (prev) {
        ctx.strokeStyle = hexToRgba(s.color, 0.1 + 0.6 * (1 - (f - t) / TRAIL_FRAMES));
        ctx.beginPath();
        ctx.moveTo(prev[0], prev[1]);
        ctx.lineTo(pt[0], pt[1]);
        ctx.stroke();
      }
      prev = pt;
    }
    ctx.restore();
  }

  drawPath(ctx, p, f) {
    const { project, results } = this.app;
    const s = project.subject(p.subjectId);
    ctx.save();
    ctx.lineWidth = 1.25;
    for (const future of [false, true]) {
      ctx.strokeStyle = hexToRgba(s.color, future ? 0.35 : 0.55);
      ctx.setLineDash(future ? [4, 4] : []);
      ctx.beginPath();
      let pen = false;
      const a = future ? f : Math.max(p.start, f - PATH_FRAMES);
      const b = future ? Math.min(project.end(p) - 1, f + PATH_FRAMES) : f;
      for (let t = a; t <= b; t++) {
        const st = project.trackerState(p, t, results);
        if (!st || st.x == null || st.mode === "pending" || st.mode === "blocked" || st.lost) {
          pen = false;
          continue;
        }
        const [x, y] = this.toScreen(st.x, st.y);
        if (pen) ctx.lineTo(x, y);
        else ctx.moveTo(x, y);
        pen = true;
      }
      ctx.stroke();
    }
    ctx.restore();
  }

  label(ctx, text, x, y, bg, fg) {
    ctx.font = "600 12px system-ui, sans-serif";
    const w = ctx.measureText(text).width;
    ctx.fillStyle = bg;
    ctx.beginPath();
    ctx.roundRect(x - 4, y - 10, w + 8, 18, 4);
    ctx.fill();
    ctx.fillStyle = fg;
    ctx.textAlign = "left";
    ctx.textBaseline = "middle";
    ctx.fillText(text, x, y - 1);
  }

  setHint(text) {
    if (this._hint !== text) {
      this._hint = text;
      this.hintEl.textContent = text;
    }
  }
}
