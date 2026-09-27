// Viewer interaction modes. The viewer owns the view transform, base drawing
// and hit-testing; the active mode interprets pointer input and draws its own
// overlays. Interface: enter(), exit(), onDown/onMove/onUp(e, sx, sy),
// onKey(e) -> handled?, draw(ctx), hint(f) -> string|null.

import { falloff, fillGap, MIN_SIZE } from "./bounds.js";
import { trackerLabel } from "./project.js";
import { clamp, hexToRgba, toast } from "./util.js";

const DRAG_THRESHOLD = 3;

export class NormalMode {
  constructor(viewer) {
    this.v = viewer;
    this.app = viewer.app;
    this.name = "normal";
    this.g = null; // active gesture
  }

  enter() {}

  exit() {
    this.g = null;
  }

  // Tracker being dragged ({trackerId, x, y}) or subject offset preview ({sid, x, y}).
  get trackerDrag() {
    return this.g?.type === "tracker" && this.g.moved && this.g.pos ? { trackerId: this.g.trackerId, x: this.g.pos[0], y: this.g.pos[1] } : null;
  }

  get offsetDrag() {
    return this.g?.type === "offset" && this.g.moved && this.g.pos ? { sid: this.g.sid, x: this.g.pos[0], y: this.g.pos[1] } : null;
  }

  onDown(e, sx, sy) {
    const { app, v } = this;
    const f = v.frame();
    const hit = v.hitTest(sx, sy);
    if (e.shiftKey) {
      this.g = { type: "marquee", kind: "template", x0: sx, y0: sy, x1: sx, y1: sy };
    } else if (e.ctrlKey || e.metaKey) {
      this.g = { type: "ctrl", hit, sx, sy, alt: e.altKey, moved: false };
    } else if (hit?.kind === "tracker") {
      const p = hit.p;
      if (app.isSelected(p.id) && app.selTrackers.size > 1) app.select({ trackers: [p.id], add: true });
      else app.select({ subject: p.subjectId, tracker: p.id });
      this.g = { type: "tracker", trackerId: p.id, frame: f, sx, sy, moved: false };
    } else if (hit?.kind === "subject") {
      // Becomes an offset drag only once the mouse moves (see onMove / onUp).
      const st = app.project.subjectState(hit.s, f, app.results);
      this.g = { type: "offset", sid: hit.s.id, near: hit.near, frame: f, sx, sy, x0: st.x, y0: st.y, moved: false };
    } else {
      this.g = { type: "empty", sx, sy, frame: f, moved: false };
    }
    return true;
  }

  onMove(e, sx, sy) {
    const g = this.g;
    if (!g) return false;
    const far = Math.hypot(sx - (g.sx ?? g.x0), sy - (g.sy ?? g.y0)) > DRAG_THRESHOLD;
    if (g.type === "marquee") {
      g.x1 = sx;
      g.y1 = sy;
      if (g.kind === "end-in" || g.kind === "end-out") g.kind = e.altKey ? "end-out" : "end-in";
    } else if (g.type === "ctrl") {
      if (far) this.g = { type: "marquee", kind: e.altKey ? "end-out" : "end-in", x0: g.sx, y0: g.sy, x1: sx, y1: sy };
    } else if (g.type === "empty") {
      if (far) this.g = { type: "marquee", kind: "select", x0: g.sx, y0: g.sy, x1: sx, y1: sy };
    } else if (g.type === "tracker" || g.type === "offset") {
      if (!g.moved && far) {
        g.moved = true;
        this.v.canvas.classList.add("dragging");
        if (g.type === "offset") this.app.select({ subject: g.sid, tracker: null });
      }
      if (g.moved) {
        if (g.type === "tracker") g.pos = this.v.toSource(sx, sy);
        else g.pos = [g.x0 + (sx - g.sx) / this.v.s, g.y0 + (sy - g.sy) / this.v.s];
      }
    }
    this.v.requestDraw();
    return true;
  }

  onUp(e, sx, sy) {
    const g = this.g;
    this.g = null;
    this.v.canvas.classList.remove("dragging");
    if (!g) return false;
    const { app, v } = this;
    const { project } = app;
    if (g.type === "marquee") {
      this.finishMarquee(g);
    } else if (g.type === "ctrl") {
      if (g.hit?.kind === "tracker") app.select({ tracker: g.hit.p.id, toggle: true });
    } else if (g.type === "empty") {
      this.addPoint(g.sx, g.sy, g.frame);
    } else if (g.type === "tracker" && g.moved && g.pos) {
      const [x, y] = g.pos;
      const p = project.tracker(g.trackerId);
      const inManual = p && project.manualRangeAt(p, g.frame);
      if (p && !inManual && !project.isInside(x, y)) {
        // Trackers can't follow anything off-screen: switch to manual from here.
        app.edit("Move off-frame", () => project.toggleManualAt(g.trackerId, g.frame, { x, y }));
        toast(`${trackerLabel(p)} moved off-frame, so it's manually animated from frame ${g.frame}. Press M on a frame where it's back inside to resume auto tracking.`);
      } else {
        app.edit(inManual ? "Set keyframe" : "Re-anchor tracker", () => project.setKey(g.trackerId, g.frame, x, y));
      }
    } else if (g.type === "offset" && !g.moved) {
      const p = g.near;
      if (!p) this.addPoint(g.sx, g.sy, g.frame);
      else if (app.isSelected(p.id) && app.selTrackers.size > 1) app.select({ trackers: [p.id], add: true });
      else app.select({ subject: p.subjectId, tracker: p.id });
    } else if (g.type === "offset" && g.moved && g.pos) {
      const s = project.subject(g.sid);
      const st = s && project.subjectState(s, g.frame, app.results);
      if (st?.rawX != null) {
        app.setOffsetKey(s.id, g.frame, g.pos[0] - st.rawX - st.fineX, g.pos[1] - st.rawY - st.fineY);
        toast(`${s.name}: offset keyed on frame ${g.frame}. Delete keys in the sidebar; [ ] jump between them.`);
      }
    }
    v.requestDraw();
    return true;
  }

  addPoint(sx, sy, f) {
    const { app, v } = this;
    const [x, y] = v.toSource(sx, sy);
    const inside = app.project.isInside(x, y);
    const subjectId = app.ensureSubject();
    let p;
    app.edit("Add point", () => {
      p = app.project.addPoint(subjectId, f, x, y, !inside);
    });
    app.select({ subject: subjectId, tracker: p.id });
    if (!inside) toast(`P${p.id} is outside the frame, so it's manually animated — drag it on other frames to key its motion.`);
  }

  finishMarquee(g) {
    const { app, v } = this;
    const small = Math.abs(g.x1 - g.x0) <= DRAG_THRESHOLD && Math.abs(g.y1 - g.y0) <= DRAG_THRESHOLD;
    if (small) return;
    const [ax, ay] = v.toSource(Math.min(g.x0, g.x1), Math.min(g.y0, g.y1));
    const [bx, by] = v.toSource(Math.max(g.x0, g.x1), Math.max(g.y0, g.y1));
    const rect = { x0: ax, y0: ay, x1: bx, y1: by };
    if (g.kind === "select") {
      const ids = v.trackersIn(rect);
      app.select({ trackers: ids });
      if (ids.length) toast(`${ids.length} tracker(s) selected — Shift+G tracks just these.`);
    } else if (g.kind === "end-in") app.endTrackersInBox(rect, true);
    else if (g.kind === "end-out") app.endTrackersInBox(rect, false);
    else if (g.kind === "template") app.openTemplateEditor(rect);
  }

  onKey() {
    return false;
  }

  draw(ctx) {
    const g = this.g;
    if (g?.type !== "marquee") return;
    const x = Math.min(g.x0, g.x1);
    const y = Math.min(g.y0, g.y1);
    const w = Math.abs(g.x1 - g.x0);
    const h = Math.abs(g.y1 - g.y0);
    const style = {
      select: ["rgba(34,211,238,.08)", "rgba(34,211,238,.85)", "select"],
      template: ["rgba(163,230,53,.10)", "rgba(163,230,53,.95)", "template box"],
      "end-in": ["rgba(244,63,94,.18)", "rgba(244,63,94,.95)", "end inside"],
      "end-out": ["rgba(244,63,94,.18)", "rgba(244,63,94,.95)", "end outside"],
    }[g.kind];
    ctx.save();
    if (g.kind === "end-out") {
      ctx.fillStyle = style[0];
      ctx.beginPath();
      ctx.rect(0, 0, this.v.cw, this.v.ch);
      ctx.rect(x, y, w, h);
      ctx.fill("evenodd");
    } else {
      ctx.fillStyle = style[0];
      ctx.fillRect(x, y, w, h);
    }
    ctx.strokeStyle = style[1];
    ctx.lineWidth = 1;
    ctx.setLineDash(g.kind === "template" ? [] : [4, 3]);
    ctx.strokeRect(x + 0.5, y + 0.5, w, h);
    ctx.setLineDash([]);
    this.v.label(ctx, style[2], x, y - 8, "rgba(0,0,0,.7)", style[1]);
    ctx.restore();
  }

  hint(f) {
    const { project, results } = this.app;
    const app = this.app;
    if (!project.subjects.length) return "Click the video to place a point (creates a subject), or press N to create and name one first.";
    const n = app.selTrackers.size;
    if (n > 1) return `${n} trackers selected — Shift+G tracks just these · M / E / Del apply to all · Ctrl+click toggles · Esc clears.`;
    const sel = app.selTracker != null ? project.tracker(app.selTracker) : null;
    if (sel) {
      const name = trackerLabel(sel);
      const st = project.trackerState(sel, f, results);
      if (!st) return `${name} doesn't exist on this frame (lives ${sel.start}–${sel.end ?? "end"}). Click to add a new point.`;
      if (st.mode === "manual") return `${name} is manually animated here — drag to key it (anywhere, even off-frame) · M stops manual here.`;
      if (st.mode === "blocked") return `${name} would resume tracking from off-frame — drag it inside the frame to re-anchor, or press M to keep it manual.`;
      if (st.mode === "pending") return `${name} needs tracking here — G tracks everything, Shift+G just the selection · drag to re-anchor.`;
      return `${name}: drag to re-anchor (off-frame switches to manual) · M manual from here · double-click to end it here.`;
    }
    const subj = project.subject(app.selSubject);
    if (subj) {
      return `“${subj.name}”: click to add a point · drag to box-select · Ctrl+drag ends trackers inside (Ctrl+Alt: outside) · ` +
        "Shift+drag draws a template box · drag the subject marker's ring to offset it.";
    }
    return "Select a subject (1–9) to add points to it.";
  }
}

// ---- Bounds mode (B): edit the selected subject's animated box ---------------------------

const HANDLE = 7;
const CURSORS = { move: "move", n: "ns-resize", s: "ns-resize", e: "ew-resize", w: "ew-resize",
  ne: "nesw-resize", sw: "nesw-resize", nw: "nwse-resize", se: "nwse-resize" };
const FALLOFF_KEY = "cotrack.boundsFalloff2";
export const FALLOFF_DEFAULT_S = 0.2;

export function boundsFalloffSeconds() {
  return clamp(Number(localStorage.getItem(FALLOFF_KEY)) || FALLOFF_DEFAULT_S, 0.02, 60);
}

export function setBoundsFalloffSeconds(sec) {
  localStorage.setItem(FALLOFF_KEY, String(clamp(sec, 0.02, 60)));
}

// Nudge the manual layer over the effective bounds: frames that only have
// recorded-pass bounds are seeded with the composite first, so a manual
// correction can build on top of the passes (manual wins where it is set).
export function nudgeManual(project, s, store, f, delta, R, N) {
  let a = -1;
  let b = -1;
  const r = Math.max(1, Math.round(R));
  for (let g = Math.max(0, f - r + 1); g <= Math.min(N - 1, f + r - 1); g++) {
    const w = falloff(g - f, r);
    if (!w) continue;
    let v = store.get(g);
    if (!v) {
      const eff = project.boundsAt(s, g);
      if (!eff) continue;
      v = eff.slice();
    }
    for (let c = 0; c < 4; c++) v[c] += delta[c] * w;
    v[2] = Math.max(MIN_SIZE, v[2]);
    v[3] = Math.max(MIN_SIZE, v[3]);
    store.set(g, v);
    if (a < 0) a = g;
    b = g;
  }
  return a < 0 ? null : [a, b];
}

export class BoundsMode {
  constructor(viewer) {
    this.v = viewer;
    this.app = viewer.app;
    this.name = "bounds";
    this.g = null;
  }

  get subject() {
    return this.app.project.subject(this.app.selSubject);
  }

  // Falloff radius in frames (the change fades to zero R frames away).
  get R() {
    return Math.max(1, Math.round(boundsFalloffSeconds() * (this.app.meta?.fps || 30)));
  }

  enter() {
    this.app.player.pause();
  }

  exit() {
    this.g = null;
    this.v.canvas.style.cursor = "";
  }

  part(sx, sy) {
    const s = this.subject;
    const b = s && this.app.project.boundsAt(s, this.v.frame());
    if (!b) return null;
    const r = this.v.boundsRect(b);
    const nx = Math.abs(sx - r.x) <= HANDLE ? "w" : Math.abs(sx - r.x - r.w) <= HANDLE ? "e" : "";
    const ny = Math.abs(sy - r.y) <= HANDLE ? "n" : Math.abs(sy - r.y - r.h) <= HANDLE ? "s" : "";
    const inX = sx >= r.x - HANDLE && sx <= r.x + r.w + HANDLE;
    const inY = sy >= r.y - HANDLE && sy <= r.y + r.h + HANDLE;
    if (!inX || !inY) return null;
    if (ny + nx) return ny + nx;
    return sx > r.x && sx < r.x + r.w && sy > r.y && sy < r.y + r.h ? "move" : null;
  }

  delta(g, sx, sy) {
    const dx = (sx - g.sx) / this.v.s;
    const dy = (sy - g.sy) / this.v.s;
    if (g.part === "move") return [dx, dy, 0, 0];
    const d = [0, 0, 0, 0];
    if (g.part.includes("e")) { d[0] += dx / 2; d[2] += dx; }
    if (g.part.includes("w")) { d[0] += dx / 2; d[2] -= dx; }
    if (g.part.includes("s")) { d[1] += dy / 2; d[3] += dy; }
    if (g.part.includes("n")) { d[1] += dy / 2; d[3] -= dy; }
    return d;
  }

  onDown(e, sx, sy) {
    const s = this.subject;
    if (!s) return false;
    const f = this.v.frame();
    const part = this.part(sx, sy);
    if (part) {
      this.g = { type: "edit", sid: s.id, part, f, sx, sy, b0: this.app.project.boundsAt(s, f), d: [0, 0, 0, 0] };
    } else {
      this.g = { type: "draw", sid: s.id, f, x0: sx, y0: sy, x1: sx, y1: sy };
    }
    return true;
  }

  onMove(e, sx, sy) {
    const g = this.g;
    if (!g) {
      const part = this.part(sx, sy);
      this.v.canvas.style.cursor = part ? CURSORS[part] : "crosshair";
      return true;
    }
    if (g.type === "edit") g.d = this.delta(g, sx, sy);
    else {
      g.x1 = sx;
      g.y1 = sy;
    }
    this.v.requestDraw();
    this.app.timeline.requestDraw();
    return true;
  }

  onUp(e, sx, sy) {
    const g = this.g;
    this.g = null;
    if (!g) return true;
    const { app } = this;
    const N = app.meta.frameCount;
    const R = this.R;
    const subj = app.project.subject(g.sid);
    if (g.type === "edit") {
      const d = this.delta(g, sx, sy);
      if (Math.hypot(sx - g.sx, sy - g.sy) > DRAG_THRESHOLD) {
        app.editBounds(g.part === "move" ? "Move bounds" : "Resize bounds", g.sid,
          (store) => nudgeManual(app.project, subj, store, g.f, d, R, N));
      }
    } else if (Math.abs(g.x1 - g.x0) > DRAG_THRESHOLD && Math.abs(g.y1 - g.y0) > DRAG_THRESHOLD) {
      const [ax, ay] = this.v.toSource(Math.min(g.x0, g.x1), Math.min(g.y0, g.y1));
      const [bx, by] = this.v.toSource(Math.max(g.x0, g.x1), Math.max(g.y0, g.y1));
      const box = [(ax + bx) / 2, (ay + by) / 2, bx - ax, by - ay];
      const old = app.project.boundsAt(subj, g.f);
      if (old) {
        app.editBounds("Redraw bounds", g.sid, (store) =>
          nudgeManual(app.project, subj, store, g.f, box.map((v, i) => v - old[i]), R, N));
      } else {
        const range = app.editBounds("Draw bounds", g.sid, (store) => fillGap(store, g.f, box, N));
        if (range) toast(`${subj.name}: bounds set on frames ${range[0]}–${range[1]}. Drag the box on other frames to follow the subject; P records it by following with the mouse.`);
      }
    }
    this.v.requestDraw();
    this.app.timeline.requestDraw();
    return true;
  }

  onWheel(e) {
    if (!this.g && !e.altKey) return false;
    setBoundsFalloffSeconds(boundsFalloffSeconds() * Math.exp(-(e.deltaY || e.deltaX) * 0.002));
    this.v.requestDraw();
    this.app.timeline.requestDraw();
    this.app.sidebar.renderStatus();
    return true;
  }

  onKey(e) {
    if (e.key === "Escape" || ((e.key === "b" || e.key === "B") && !e.ctrlKey && !e.metaKey)) {
      this.v.setMode(null);
      return true;
    }
    return false;
  }

  previewBounds(s, f) {
    const g = this.g;
    if (g?.type !== "edit" || s.id !== g.sid || !g.b0) return null;
    const b = this.app.project.boundsAt(s, f);
    if (!b) return null;
    const w = falloff(f - g.f, this.R);
    const out = b.map((v, i) => v + g.d[i] * w);
    out[2] = Math.max(MIN_SIZE, out[2]);
    out[3] = Math.max(MIN_SIZE, out[3]);
    return out;
  }

  // Timeline overlay: the falloff curve around the edited frame.
  falloffOverlay() {
    return this.subject ? { f: this.g?.f ?? this.v.frame(), R: this.R, active: !!this.g } : null;
  }

  // The box center's path over ±R frames (with the drag previewed).
  drawUnder(ctx) {
    const s = this.subject;
    if (!s) return;
    const f = this.v.frame();
    const R = this.R;
    const { project } = this.app;
    ctx.save();
    ctx.strokeStyle = hexToRgba(s.color, 0.7);
    ctx.lineWidth = 1.25;
    ctx.setLineDash([3, 3]);
    ctx.beginPath();
    let pen = false;
    const step = Math.max(1, Math.floor(R / 150));
    for (let t = Math.max(0, f - R); t <= Math.min(this.app.meta.frameCount - 1, f + R); t += step) {
      const b = this.previewBounds(s, t) ?? project.boundsAt(s, t);
      if (!b) {
        pen = false;
        continue;
      }
      const [x, y] = this.v.toScreen(b[0], b[1]);
      if (pen) ctx.lineTo(x, y);
      else ctx.moveTo(x, y);
      pen = true;
    }
    ctx.stroke();
    ctx.restore();
  }

  draw(ctx) {
    const g = this.g;
    const s = this.subject;
    if (!s) return;
    if (g?.type === "draw") {
      const x = Math.min(g.x0, g.x1);
      const y = Math.min(g.y0, g.y1);
      ctx.save();
      ctx.fillStyle = hexToRgba(s.color, 0.08);
      ctx.fillRect(x, y, Math.abs(g.x1 - g.x0), Math.abs(g.y1 - g.y0));
      ctx.strokeStyle = hexToRgba(s.color, 0.95);
      ctx.setLineDash([4, 3]);
      ctx.strokeRect(x + 0.5, y + 0.5, Math.abs(g.x1 - g.x0), Math.abs(g.y1 - g.y0));
      ctx.restore();
      return;
    }
    const f = this.v.frame();
    const b = this.previewBounds(s, f) ?? this.app.project.boundsAt(s, f);
    if (!b) return;
    const r = this.v.boundsRect(b);
    ctx.save();
    ctx.fillStyle = "#fff";
    ctx.strokeStyle = "rgba(0,0,0,.8)";
    for (const [hx, hy] of [[0, 0], [0.5, 0], [1, 0], [0, 0.5], [1, 0.5], [0, 1], [0.5, 1], [1, 1]]) {
      ctx.beginPath();
      ctx.rect(r.x + r.w * hx - 3.5, r.y + r.h * hy - 3.5, 7, 7);
      ctx.fill();
      ctx.stroke();
    }
    ctx.restore();
  }

  hint(f) {
    const s = this.subject;
    if (!s) return "Bounds: select a subject first.";
    const R = this.R;
    const sec = boundsFalloffSeconds();
    const fall = `changes fade over ±${R} frames (${sec < 1 ? sec.toFixed(2) : sec.toFixed(1)} s; wheel while dragging or Alt+wheel adjusts)`;
    if (!this.app.project.boundsAt(s, f)) {
      return `Bounds for “${s.name}”: drag a box around the subject — it fills this gap (the whole video if there are none yet) · P records bounds by following with the mouse · Esc done.`;
    }
    return `Bounds for “${s.name}”: drag inside to move, edges/corners to resize, outside to redraw here · ${fall} · Esc or B done.`;
  }
}
