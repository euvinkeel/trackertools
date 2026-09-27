// Finetune mode (R): additive, keyed correction layers on top of the pushed
// subject position. Dragging is relative (pointer-lock): movement is added to
// the layer's held value at the current frame, Shift is 1/10 speed, arrows step
// frames while holding the value, and Q shows a targeting loupe. Each drag is
// one undoable edit. The maths lives in Project.layerSum (evalKeyed holds before
// the first and after the last key). See editor/PLAN.md §9.

import { evalKeyed } from "./chunks.js";
import { clamp, hexToRgba, toast } from "./util.js";

const LOUPE = 170;
const LOUPE_R = 42; // canvas pixels of context shown in the loupe

export class FinetuneMode {
  constructor(viewer) {
    this.v = viewer;
    this.app = viewer.app;
    this.name = "finetune";
    this.session = null;
    this.loupe = false;
    this.loupeAt = null;
    this._onUp = (e) => {
      if ((e.key === "q" || e.key === "Q") && this.loupe) {
        this.loupe = false;
        this.v.requestDraw();
      }
    };
    window.addEventListener("keyup", this._onUp, true);
  }

  get subject() {
    return this.app.project.subject(this.app.selSubject);
  }

  get layer() {
    const s = this.subject;
    return s?.layers.find((l) => l.id === s.activeLayer) || s?.layers[0] || null;
  }

  enter() {
    this.app.player.pause();
    const s = this.subject;
    if (s && !s.layers.length) this.app.addFinetuneLayer(s.id, { silent: true });
    const layer = this.layer;
    if (s && layer && s.activeLayer !== layer.id) this.app.setActiveLayer(s.id, layer.id, { silent: true });
  }

  exit() {
    this.commit();
    this.loupe = false;
    this.loupeAt = null;
  }

  // ---- input --------------------------------------------------------------------------

  onDown(e, sx, sy) {
    const app = this.app;
    const s = this.subject;
    if (!s) return false;
    const layer = this.layer;
    if (!layer) {
      toast("Finetune: add a layer first (sidebar → Finetune → + Layer).", "error");
      return true;
    }
    const f = this.v.frame();
    const st = app.project.subjectState(s, f, app.results);
    if (st.x == null) {
      toast("Nothing to nudge here — the subject has no position on this frame.");
      return true;
    }
    const store = app.project.store(layer.keys);
    const held = store ? evalKeyed(store, f) : null;
    this.session = {
      sid: s.id, layerId: layer.id, f, last: [sx, sy],
      value: [held?.[0] ?? 0, held?.[1] ?? 0], pending: new Map(), moved: false,
    };
    this.v.canvas.classList.add("dragging");
    return true;
  }

  onMove(e, sx, sy) {
    const g = this.session;
    if (!g) return true;
    const k = e.shiftKey ? 0.1 : 1;
    g.value[0] += ((sx - g.last[0]) / this.v.s) * k;
    g.value[1] += ((sy - g.last[1]) / this.v.s) * k;
    g.last = [sx, sy];
    g.moved = true;
    g.pending.set(g.f, g.value.slice());
    this.loupeAt = [sx, sy];
    this.v.requestDraw();
    this.app.inspector.requestRender();
    return true;
  }

  onUp() {
    this.commit();
    this.v.canvas.classList.remove("dragging");
    return true;
  }

  onKey(e) {
    if (e.key === "q" || e.key === "Q") {
      this.loupe = true;
      this.loupeAt = this.v.hoverPoint?.() ?? this.loupeAt;
      this.v.requestDraw();
      return true;
    }
    if (e.key === "Escape" || ((e.key === "r" || e.key === "R") && !e.ctrlKey && !e.metaKey)) {
      this.commit();
      this.v.setMode(null);
      return true;
    }
    if (this.session && (e.key === "ArrowLeft" || e.key === "ArrowRight")) {
      // Step frames while holding the value: each stepped frame gets a key.
      const dir = e.key === "ArrowLeft" ? -1 : 1;
      const f = clamp(this.session.f + dir, 0, this.app.meta.frameCount - 1);
      this.session.f = f;
      this.session.pending.set(f, this.session.value.slice());
      this.app.seek(f);
      this.v.requestDraw();
      return true;
    }
    return false;
  }

  onWheel() {
    return false;
  }

  // ---- commit -------------------------------------------------------------------------

  commit() {
    const g = this.session;
    this.session = null;
    if (!g || !g.moved || !g.pending.size) return;
    const s = this.app.project.subject(g.sid);
    const layer = s?.layers.find((l) => l.id === g.layerId);
    const store = layer && this.app.project.store(layer.keys);
    if (!store) return;
    const pending = [...g.pending.entries()];
    this.app.edit("Finetune nudge", () => {
      for (const [f, v] of pending) store.set(f, v);
      this.app.project.touch(Math.min(...pending.map(([f]) => f)));
    });
    toast(`${layer.name}: ${pending.length} key(s) nudged to ${g.value[0].toFixed(1)}, ${g.value[1].toFixed(1)} px.`);
  }

  // The nudge being dragged, so the viewer can show the final position live.
  finePreview(s, f) {
    const g = this.session;
    if (!g || g.sid !== s.id) return null;
    return g.pending.get(f) ?? null;
  }

  // ---- drawing ------------------------------------------------------------------------

  draw(ctx) {
    const g = this.session;
    if (g) {
      const [x, y] = g.last;
      this.v.label(ctx, `Δ ${g.value[0].toFixed(1)}, ${g.value[1].toFixed(1)} px`, x + 14, y - 14, "rgba(5,7,10,.9)", "#e6e8ec");
    }
    this.drawLoupe(ctx);
  }

  drawLoupe(ctx) {
    if (!this.loupe || !this.loupeAt) return;
    const { cw, ch } = this.v;
    const [mx, my] = this.loupeAt;
    const k = this.v.canvas.width / Math.max(1, cw);
    const size = Math.min(LOUPE, cw - 16, ch - 16);
    const dx = clamp(mx + 24, 8, cw - size - 8);
    const dy = clamp(my - size - 24, 8, ch - size - 8);
    ctx.save();
    ctx.beginPath();
    ctx.arc(dx + size / 2, dy + size / 2, size / 2, 0, Math.PI * 2);
    ctx.save();
    ctx.clip();
    ctx.drawImage(this.v.canvas, (mx - LOUPE_R) * k, (my - LOUPE_R) * k, 2 * LOUPE_R * k, 2 * LOUPE_R * k, dx, dy, size, size);
    ctx.restore();
    ctx.strokeStyle = "rgba(0,0,0,.85)";
    ctx.lineWidth = 4;
    ctx.stroke();
    ctx.strokeStyle = "#22d3ee";
    ctx.lineWidth = 1.75;
    ctx.stroke();
    // Crosshair at the center of the magnified region.
    const cx = dx + size / 2;
    const cy = dy + size / 2;
    ctx.strokeStyle = "rgba(250,204,21,.95)";
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    ctx.moveTo(cx - 10, cy); ctx.lineTo(cx + 10, cy);
    ctx.moveTo(cx, cy - 10); ctx.lineTo(cx, cy + 10);
    ctx.stroke();
    ctx.restore();
    this.v.label(ctx, "loupe (Q)", dx + 6, dy + 12, "rgba(5,7,10,.85)", "#22d3ee");
  }

  hint(f) {
    const s = this.subject;
    if (!s) return "Finetune: select a subject first.";
    const layer = this.layer;
    if (!layer) return "Finetune: add a layer in the sidebar (+ Layer), then drag to nudge.";
    const st = this.app.project.subjectState(s, f, this.app.results);
    if (this.session) return `Nudging “${layer.name}” — Shift = 1/10 speed · ← → step frames and hold the value · release to commit (one undo step).`;
    return `Finetune “${layer.name}” (${(layer.weight * 100).toFixed(0)}%): drag anywhere on the frame to nudge it relatively · ` +
      `Shift precision · Q loupe · ← → step · weight/stack in the sidebar · Esc or R done.` +
      (st.x == null ? " No subject position on this frame." : "");
  }
}
