import { api } from "./api.js";
import { listPatterns, savePattern } from "./patterns.js";
import { decodeMask, encodeMask, maskFromBorder } from "./templates.js";
import { clamp, toast } from "./util.js";

const PNG_PREFIX = /^data:image\/png;base64,/;

export class TemplateEditor {
  constructor(app) {
    this.app = app;
    this.el = document.getElementById("template-editor");
    this.canvas = this.el.querySelector("canvas");
    this.ctx = this.canvas.getContext("2d");
    this.el.addEventListener("click", (e) => {
      if (e.target === this.el || e.target.closest('[data-action="cancel"]')) this.close();
      const action = e.target.closest("[data-action]")?.dataset.action;
      if (action === "save") this.save();
      if (action === "save-lib") this.saveToLibrary();
      if (action === "fill" || action === "clear") {
        this.mask.fill(action === "fill" ? 1 : 0); this.draw();
      }
      if (action === "auto") {
        this.mask = maskFromBorder(this.pixels.data, this.w, this.h, Number(this.el.querySelector('[name="tolerance"]').value));
        this.draw();
      }
    });
    this.el.querySelectorAll('[name="tool"], [name="brush-size"], [name="new-tracker"]').forEach((el) => el.addEventListener("input", () => this.draw()));
    this.canvas.addEventListener("contextmenu", (e) => e.preventDefault());
    this.canvas.addEventListener("pointerdown", (e) => {
      if (e.button !== 0 && e.button !== 2) return;
      this.canvas.setPointerCapture(e.pointerId);
      this.paint(e);
    });
    this.canvas.addEventListener("pointermove", (e) => {
      if (this.canvas.hasPointerCapture(e.pointerId)) this.paint(e);
    });
    this.canvas.addEventListener("pointerup", (e) => this.canvas.releasePointerCapture(e.pointerId));
    this._onKey = (e) => {
      if (this.el.classList.contains("hidden")) return;
      if (e.key === "Escape") this.close();
      else if (e.key === "Enter" && e.target.tagName !== "BUTTON") this.save();
      else return;
      e.preventDefault(); e.stopImmediatePropagation();
    };
    window.addEventListener("keydown", this._onKey, true);
  }

  get active() { return !this.el.classList.contains("hidden"); }

  // rect is a source-space box ({x0,y0,x1,y1}); existing edits a look; pattern
  // preloads a saved library pattern's pixels/mask/hotspot (for "Use").
  async open(rect, tracker = null, existing = null, pattern = null) {
    if (!this.app.meta) return;
    this.close();
    const meta = this.app.meta;
    const f = existing?.f ?? this.app.editFrame();
    const src = pattern || existing?.tmpl ? (pattern || existing) : null;
    const x = existing?.x ?? (src ? clamp(Math.floor(rect.x0), 0, meta.width - 1) : clamp(Math.floor(rect.x0), 0, meta.width - 1));
    const y = existing?.y ?? clamp(Math.floor(rect.y0), 0, meta.height - 1);
    const w = existing?.w ?? (src ? src.w : clamp(Math.ceil(rect.x1) - x, 1, meta.width - x));
    const h = existing?.h ?? (src ? src.h : clamp(Math.ceil(rect.y1) - y, 1, meta.height - y));
    if (w < 4 || h < 4 || w > 512 || h > 512) {
      toast("Draw a template box between 4 and 512 pixels on each side.", "error");
      return;
    }
    this.w = w; this.h = h; this.x = x; this.y = y; this.f = f;
    this.tracker = tracker; this.existing = existing;
    this.pattern = pattern || null;
    this.mask = decodeMask(existing?.mask ?? pattern?.mask, w, h);
    const pos = tracker && !existing && !pattern && this.app.project.trackerState(tracker, f, this.app.results);
    this.hx = existing?.hx ?? pattern?.hx ?? (pos?.x != null ? pos.x - x : w / 2);
    this.hy = existing?.hy ?? pattern?.hy ?? (pos?.y != null ? pos.y - y : h / 2);
    this.el.querySelector("h2").textContent = existing
      ? `Edit look for T${tracker.id}`
      : pattern
        ? tracker ? `Use pattern on T${tracker.id}` : "New tracker from pattern"
        : tracker ? `Add look to T${tracker.id}` : "New template tracker";
    const sw = this.el.querySelector('[name="new-tracker"]');
    sw.closest("label").classList.toggle("hidden", !tracker || !!existing || !!pattern);
    sw.checked = !tracker;
    const nameInput = this.el.querySelector('[name="lib-name"]');
    nameInput.value = pattern?.name || "";
    this.el.querySelector(".template-info").textContent = `Frame ${f} · ${w}×${h} source pixels · set hotspot at the point you want to follow` +
      (src ? ` · using saved pattern “${src.name || "pattern"}”` : "");
    this.el.querySelector('[name="tool"][value="brush"]').checked = true;
    const scale = Math.min(18, 620 / w, 460 / h);
    const dpr = window.devicePixelRatio || 1;
    this.canvas.style.width = `${Math.round(w * scale)}px`;
    this.canvas.style.height = `${Math.round(h * scale)}px`;
    this.canvas.width = Math.round(w * scale * dpr);
    this.canvas.height = Math.round(h * scale * dpr);
    this.el.classList.remove("hidden");
    const token = this.token = (this.token || 0) + 1;
    this.el.querySelector(".template-wait").textContent = "Loading pixels…";
    try {
      const img = new Image();
      // A saved pattern carries its own pixels; a normal look is cut from the
      // video at the original resolution.
      img.src = src ? `data:image/png;base64,${src.tmpl}` : api.cropUrl(meta.id, f, x, y, w, h);
      await img.decode();
      if (token !== this.token || meta !== this.app.meta) return;
      this.source = document.createElement("canvas");
      this.source.width = w; this.source.height = h;
      const ctx = this.source.getContext("2d", { willReadFrequently: true });
      ctx.drawImage(img, 0, 0, w, h);
      this.pixels = ctx.getImageData(0, 0, w, h);
      this.tint = document.createElement("canvas");
      this.tint.width = w; this.tint.height = h;
      this.el.querySelector(".template-wait").textContent = "";
      this.draw();
    } catch (err) {
      if (token === this.token) this.el.querySelector(".template-wait").textContent = `Pixels failed: ${err.message}`;
    }
  }

  // Open the editor on a saved pattern, positioned on the tracker (or the
  // subject center) so the hotspot can be placed explicitly.
  async openPattern(pattern, tracker = null) {
    const app = this.app;
    if (!app.meta) return;
    const f = app.editFrame();
    const st = tracker ? app.project.trackerState(tracker, f, app.results) : null;
    const cx = st?.x ?? app.meta.width / 2;
    const cy = st?.y ?? app.meta.height / 2;
    const x = clamp(Math.round(cx - pattern.hx), 0, Math.max(0, app.meta.width - pattern.w));
    const y = clamp(Math.round(cy - pattern.hy), 0, Math.max(0, app.meta.height - pattern.h));
    await this.open({ x0: x, y0: y, x1: x + pattern.w, y1: y + pattern.h }, tracker, null, pattern);
  }

  paint(e) {
    if (!this.pixels) return;
    const bounds = this.canvas.getBoundingClientRect();
    const x = (e.clientX - bounds.left) * this.w / bounds.width;
    const y = (e.clientY - bounds.top) * this.h / bounds.height;
    const tool = e.button === 2 || e.buttons === 2 ? "eraser" : this.el.querySelector('[name="tool"]:checked').value;
    if (tool === "hotspot") {
      const snap = (v) => e.shiftKey ? Math.round(v * 100) / 100 : Math.round(v * 2) / 2;
      this.hx = clamp(snap(x), 0, this.w);
      this.hy = clamp(snap(y), 0, this.h);
    } else {
      const radius = Number(this.el.querySelector('[name="brush-size"]').value) / 2;
      for (let iy = Math.max(0, Math.floor(y - radius)); iy < Math.min(this.h, Math.ceil(y + radius)); iy++) {
        for (let ix = Math.max(0, Math.floor(x - radius)); ix < Math.min(this.w, Math.ceil(x + radius)); ix++) {
          if (Math.hypot(ix + 0.5 - x, iy + 0.5 - y) <= Math.max(0.7, radius)) this.mask[iy * this.w + ix] = tool === "brush" ? 1 : 0;
        }
      }
    }
    this.draw();
  }

  draw() {
    if (!this.pixels || !this.active) return;
    const { ctx, w, h } = this;
    const tint = new ImageData(w, h);
    for (let i = 0; i < this.mask.length; i++) {
      if (!this.mask[i]) {
        tint.data[4 * i] = 155;
        tint.data[4 * i + 3] = 165;
      }
    }
    this.tint.getContext("2d").putImageData(tint, 0, 0);
    const k = this.canvas.width / w;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.imageSmoothingEnabled = false;
    ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);
    ctx.drawImage(this.source, 0, 0, this.canvas.width, this.canvas.height);
    ctx.drawImage(this.tint, 0, 0, this.canvas.width, this.canvas.height);
    // Dark halo under a bright cross so the hotspot reads on any pixels.
    const dpr = window.devicePixelRatio || 1;
    const hx = this.hx * k, hy = this.hy * k, arm = 11 * dpr;
    for (const [color, width] of [["rgba(0,0,0,.85)", 4 * dpr], ["#facc15", 1.6 * dpr]]) {
      ctx.strokeStyle = color;
      ctx.lineWidth = width;
      ctx.beginPath();
      ctx.moveTo(hx - arm, hy); ctx.lineTo(hx + arm, hy);
      ctx.moveTo(hx, hy - arm); ctx.lineTo(hx, hy + arm);
      ctx.stroke();
    }
    this.el.querySelector(".template-selected").textContent = `${this.mask.reduce((a, b) => a + b, 0)} / ${w * h} pixels · hotspot ${this.hx.toFixed(1)}, ${this.hy.toFixed(1)}`;
  }

  // The mask in storage form: "" for a full or empty mask (the whole box).
  maskValue() {
    return this.mask.every((v) => v === 1) || this.mask.every((v) => v === 0) ? "" : encodeMask(this.mask);
  }

  pixelsPng() {
    return this.source.toDataURL("image/png").replace(PNG_PREFIX, "");
  }

  // Save the current pixels/mask/hotspot to the library (updates the pattern
  // this editor was opened from, when there is one). The editor stays open.
  saveToLibrary() {
    if (!this.pixels) return;
    const name = (this.el.querySelector('[name="lib-name"]').value || "").trim()
      || this.pattern?.name || `Pattern ${listPatterns().length + 1}`;
    try {
      const entry = savePattern({
        id: this.pattern?.id, name, w: this.w, h: this.h, hx: this.hx, hy: this.hy,
        tmpl: this.pixelsPng(), mask: this.maskValue(),
      });
      this.pattern = entry;
      this.el.querySelector('[name="lib-name"]').value = entry.name;
      toast(`Saved “${entry.name}” to the pattern library (${this.app.sidebar.patternCount?.() ?? ""} available).`);
      this.app.sidebar.renderPatterns?.();
    } catch (err) {
      toast(err.message, "error");
    }
  }

  save() {
    if (!this.pixels) return;
    const look = { f: this.f, x: this.x, y: this.y, w: this.w, h: this.h,
      hx: this.hx, hy: this.hy, mask: this.maskValue() };
    if (this.pattern) {
      // Copy the (possibly edited) pixels into the look: later library edits
      // must not change trackers that already use the pattern.
      look.tmpl = this.pixelsPng();
      look.pid = this.pattern.id;
    }
    const app = this.app;
    const f = app.editFrame();
    if (this.existing) {
      app.editTemplateLook("Edit template look", [this.tracker.id], f,
        () => app.project.updateLook(this.tracker.id, this.existing.id, look));
    } else if (this.tracker && !this.el.querySelector('[name="new-tracker"]').checked) {
      // A new look only applies from here on: the frames before the cursor were
      // tracked with the previous looks and stay as they are.
      app.editTemplateLook("Add template look", [this.tracker.id], f,
        () => app.project.addLook(this.tracker.id, look));
    } else {
      const sid = app.ensureSubject();
      let p;
      app.edit("Add template tracker", () => { p = app.project.addTemplate(sid, look); });
      app.select({ subject: sid, tracker: p.id });
    }
    this.close();
    toast("Template saved. Press G to track it.");
  }

  close() {
    this.token = (this.token || 0) + 1;
    this.pixels = this.source = this.tint = null;
    this.pattern = null;
    this.el.classList.add("hidden");
  }
}
