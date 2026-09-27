import { api } from "./api.js";
import { clearBounds } from "./bounds.js";
import { exportCSV, exportFusion, exportJSON, exportNormalizedCSV } from "./exporter.js";
import { Inspector } from "./inspector.js";
import { exportPatterns, importPatterns, patternById } from "./patterns.js";
import { Player } from "./player.js";
import { Project, trackerLabel } from "./project.js";
import { regeneratePass } from "./puppeteer.js";
import { ResultStore } from "./results.js";
import { Sidebar } from "./sidebar.js";
import { Timeline } from "./timeline.js";
import { TemplateEditor } from "./template-editor.js";
import { Tracker } from "./tracker.js";
import { bytes, clamp, download, duration, escapeHtml, timecode, toast } from "./util.js";
import { Viewer } from "./viewer.js";

const $ = (id) => document.getElementById(id);
const UNDO_LIMIT = 300;

class App {
  constructor() {
    this.meta = null;
    this.project = new Project(1);
    this.results = new ResultStore();
    this.ready = false;
    this.cursor = 0;
    this.shown = -1;
    this.selSubject = null;
    this.selTracker = null; // primary selected tracker (inspector, dragging, M/E target)
    this.selTrackers = new Set(); // all selected trackers
    this.endMarkerFocus = null; // tracker whose end marker is focused in the timeline
    this.puppeteerTarget = { kind: "refine" }; // refine (new level) or take (average into level)
    this.undoStack = [];
    this.redoStack = [];
    this.objectUrl = null;
    this.dirty = false;
    this._saveTimer = null;
    this._statusTimer = null;

    this.video = $("video");
    this.player = new Player(this.video, (i) => this.onShown(i));
    this.viewer = new Viewer(this);
    this.timeline = new Timeline(this);
    this.sidebar = new Sidebar(this);
    this.inspector = new Inspector(this);
    this.tracker = new Tracker(this);
    this.templateEditor = new TemplateEditor(this);

    this.bindUI();
    this.bindKeys();
    this.bindDrop();
    this.showStart();
  }

  // ---- start screen / opening videos -------------------------------------------------------

  async showStart() {
    $("editor").classList.add("hidden");
    $("drop-screen").classList.remove("hidden");
    this.pollHealth();
    try {
      const vids = await api.videos();
      $("recent-wrap").classList.toggle("hidden", !vids.length);
      $("recent").innerHTML = vids
        .map((v) => `<button class="recent-item" data-id="${v.id}">
            <span class="name">${escapeHtml(v.name)}</span>
            <span class="badge">${v.width}×${v.height}</span>
            <span class="badge">${Math.round(v.fps * 100) / 100} fps</span>
            <span class="badge">${duration(v.duration)}</span>
            ${v.copied ? "" : `<span class="badge" title="${escapeHtml(v.sourcePath || "")}">linked</span>`}
            ${v.hasProject ? `<span class="badge proj">project</span>` : ""}
          </button>`)
        .join("");
      this._recent = vids;
    } catch {
      $("recent-wrap").classList.add("hidden");
    }
  }

  async pollHealth() {
    clearTimeout(this._healthTimer);
    try {
      const h = await api.health();
      const el = $("model-status");
      if (h.modelError) el.textContent = `Model failed to load: ${h.modelError}`;
      else if (h.modelReady) el.textContent = `● Tracker ready on ${h.gpu || h.device}`;
      else {
        el.textContent = "◌ Loading tracking model… (you can open a video meanwhile)";
        this._healthTimer = setTimeout(() => this.pollHealth(), 1500);
      }
    } catch {
      $("model-status").textContent = "Server not reachable.";
      this._healthTimer = setTimeout(() => this.pollHealth(), 3000);
    }
  }

  async openFile(file) {
    if (!file) return;
    if (!file.type.startsWith("video/") && !/\.(mp4|mkv|mov|webm|m4v|avi)$/i.test(file.name)) {
      toast(`${file.name} doesn't look like a video file.`, "error");
      return;
    }
    await this.closeVideo(false);
    const fingerprint = `${file.name}|${file.size}|${file.lastModified}`;
    this.objectUrl = URL.createObjectURL(file);
    this.enterEditor(this.objectUrl, file.name);
    try {
      let meta = await api.lookup(fingerprint);
      if (!meta) {
        this.setImport(0, `Importing ${bytes(file.size)}…`);
        meta = await api.upload(file, fingerprint, (p) => this.setImport(p, `Importing ${Math.round(p * 100)}%`));
        this.setImport(1, "Reading video…");
      }
      await this.setMeta(meta);
    } catch (err) {
      toast(err.message, "error");
      this.setImport(null);
      $("file-info").innerHTML = `<b>${escapeHtml(file.name)}</b> · import failed`;
    }
  }

  async openMeta(meta) {
    await this.closeVideo(false);
    this.enterEditor(api.fileUrl(meta.id), meta.name);
    try {
      await this.setMeta(await api.video(meta.id));
    } catch (err) {
      toast(err.message, "error");
    }
  }

  async openPath(path) {
    try {
      const meta = await api.openPath(path);
      await this.openMeta(meta);
    } catch (err) {
      toast(err.message, "error");
    }
  }

  enterEditor(src, name) {
    $("drop-screen").classList.add("hidden");
    $("editor").classList.remove("hidden");
    $("video-error").classList.add("hidden");
    $("file-info").innerHTML = `<b>${escapeHtml(name)}</b>`;
    this.ready = false;
    this.originalSrc = src;
    this.player.reset();
    this.video.src = src;
    this.video.addEventListener("loadedmetadata", () => {
      if (!this.meta) this.viewer.setSize(this.video.videoWidth || 16, this.video.videoHeight || 9);
    }, { once: true });
    this.refreshAll();
  }

  setImport(p, text) {
    const el = $("import-status");
    if (p == null) {
      el.classList.add("hidden");
      return;
    }
    el.classList.remove("hidden");
    el.querySelector(".bar div").style.width = `${Math.round(p * 100)}%`;
    el.querySelector("span").textContent = text;
  }

  async setMeta(meta) {
    this.meta = meta;
    this.project = new Project(meta.frameCount, meta.width, meta.height, meta.fps);
    this.results = new ResultStore();
    this.undoStack = [];
    this.redoStack = [];
    this.selSubject = null;
    this.selTracker = null;
    this.selTrackers = new Set();
    let cursor = 0;
    try {
      const saved = await api.loadProject(meta.id);
      if (saved) {
        this.project.load(saved.project || {});
        this.results.load(saved.results || {});
        cursor = saved.ui?.cursor ?? 0;
        this.selSubject = this.project.subject(saved.ui?.selSubject)?.id ?? this.project.subjects[0]?.id ?? null;
      }
    } catch (err) {
      toast(`Couldn't load saved project: ${err.message}`, "error");
    }
    this.cursor = clamp(cursor, 0, meta.frameCount - 1);
    this.applySource();
    this.pollProxy();
    this.viewer.setSize(meta.width, meta.height);
    this.setImport(null);
    $("file-info").innerHTML = `<b>${escapeHtml(meta.name)}</b> · ${meta.width}×${meta.height} · ${Math.round(meta.fps * 1000) / 1000} fps ·
      ${meta.frameCount.toLocaleString()} frames · ${duration(meta.duration)}${meta.copied ? "" : " · linked file"}`;
    $("frame-input").max = meta.frameCount - 1;
    $("frame-total").textContent = `/ ${(meta.frameCount - 1).toLocaleString()}`;
    this.ready = true;
    this.dirty = false;
    this.setSaveStatus(this.project.trackers.length ? "Saved" : "");
    this.tracker.reset();
    this.timeline.reset();
    this.seek(cursor);
    this.refreshAll();
    this.scheduleQuality();
  }

  // Display source: the fast-scrub proxy when available (unless the user asked
  // for the original), otherwise the original file.
  applySource() {
    const m = this.meta;
    const useProxy = m.proxy?.state === "ready" && !this.preferOriginal;
    const src = useProxy ? api.proxyUrl(m.id) : this.originalSrc;
    this.source = useProxy ? "proxy" : "original";
    this.player.setMeta({ fps: m.fps, t0: useProxy ? m.proxy.t0 || 0 : m.t0 || 0, frameCount: m.frameCount });
    if (this.video.src !== new URL(src, location.href).href) {
      const cursor = this.cursor;
      const wasMuted = this.video.muted;
      this.player.reset();
      this.video.src = src;
      this.video.muted = wasMuted;
      this.video.addEventListener("loadedmetadata", () => this.seek(cursor), { once: true });
    }
    $("video-error").classList.add("hidden");
    const btn = $("btn-source");
    btn.classList.toggle("hidden", m.proxy?.state !== "ready");
    btn.classList.toggle("is-proxy", useProxy);
    btn.textContent = useProxy ? `Proxy ${m.proxy.height}p` : "Original";
  }

  toggleSource() {
    if (this.meta?.proxy?.state !== "ready") return;
    this.preferOriginal = this.source === "proxy";
    this.applySource();
    toast(this.preferOriginal
      ? "Showing the original file (full resolution; frame stepping may be slower)."
      : "Showing the fast-scrub proxy.");
  }

  async pollProxy() {
    clearTimeout(this._proxyTimer);
    const m = this.meta;
    const el = $("proxy-status");
    if (!m || !m.needsProxy || m.proxy?.state === "ready") {
      el.classList.add("hidden");
      return;
    }
    try {
      const st = await api.proxyStatus(m.id);
      if (this.meta !== m) return;
      m.proxy = st;
      if (st.state === "ready") {
        el.classList.add("hidden");
        this.applySource();
        toast("Fast-scrub proxy ready — frame stepping is now instant.");
        return;
      }
      el.classList.remove("hidden");
      if (st.state === "error") {
        el.querySelector("span").textContent = "Proxy failed";
        el.title = st.message || "Proxy build failed";
        return;
      }
      el.querySelector(".bar div").style.width = `${Math.round((st.progress || 0) * 100)}%`;
      el.querySelector("span").textContent = st.state === "queued"
        ? "Proxy queued"
        : `Building scrub proxy ${Math.round((st.progress || 0) * 100)}%`;
      this._proxyTimer = setTimeout(() => this.pollProxy(), 1000);
    } catch {
      this._proxyTimer = setTimeout(() => this.pollProxy(), 3000);
    }
  }

  async closeVideo(showStart = true) {
    this.templateEditor.close();
    this.viewer.setMode(null);
    clearTimeout(this._proxyTimer);
    clearTimeout(this._qualityTimer);
    this._qualityTimer = null;
    this.endMarkerFocus = null;
    $("proxy-status").classList.add("hidden");
    $("btn-source").classList.add("hidden");
    this.preferOriginal = false;
    if (this.tracker.active) this.tracker.halt();
    if (this.ready && this.dirty) await this.save();
    this.ready = false;
    this.meta = null;
    this.project = new Project(1);
    this.results = new ResultStore();
    this.shown = -1;
    this.cursor = 0;
    this.video.pause();
    this.video.removeAttribute("src");
    this.video.load();
    if (this.objectUrl) URL.revokeObjectURL(this.objectUrl);
    this.objectUrl = null;
    this.tracker.reset();
    this.setImport(null);
    if (showStart) this.showStart();
  }

  // ---- frames -------------------------------------------------------------------------

  seek(f) {
    if (!this.meta) return;
    this.cursor = clamp(Math.round(f), 0, this.meta.frameCount - 1);
    this.player.seek(this.cursor);
    this.timeline.ensureVisible(this.cursor);
    this.timeline.requestDraw();
    this.updateTransport();
  }

  onShown(i) {
    this.shown = i;
    if (this.player.playing) {
      this.cursor = i;
      this.timeline.ensureVisible(i);
    }
    this.viewer.drawNow();
    this.timeline.requestDraw();
    this.updateTransport();
    this.scheduleStatusRefresh();
  }

  scheduleStatusRefresh() {
    if (this._statusTimer) return;
    this._statusTimer = setTimeout(() => {
      this._statusTimer = null;
      this.sidebar.renderStatus();
      this.inspector.render();
    }, 60);
  }

  updateTransport() {
    const input = $("frame-input");
    if (document.activeElement !== input) input.value = this.cursor;
    $("timecode").textContent = this.meta ? timecode(this.cursor, this.meta.fps) : "";
    $("btn-play").textContent = this.player.playing ? "⏸" : "▶";
  }

  // ---- selection & editing ----------------------------------------------------------------

  // select({tracker})               select just this tracker (or clear with null)
  // select({tracker, toggle: true}) add/remove a tracker from the selection
  // select({trackers, add})         select several (box select)
  select({ subject = this.selSubject, tracker = null, toggle = false, trackers = null, add = false, endFocus = false }) {
    if (!endFocus) this.endMarkerFocus = null;
    if (trackers) {
      if (!add) this.selTrackers.clear();
      for (const id of trackers) this.selTrackers.add(id);
      if (trackers.length) this.selTracker = trackers[trackers.length - 1];
      else if (!add) this.selTracker = null;
    } else if (toggle && tracker != null) {
      if (this.selTrackers.has(tracker)) {
        this.selTrackers.delete(tracker);
        if (this.selTracker === tracker) this.selTracker = [...this.selTrackers].pop() ?? null;
      } else {
        this.selTrackers.add(tracker);
        this.selTracker = tracker;
      }
    } else {
      this.selTrackers = new Set(tracker != null ? [tracker] : []);
      this.selTracker = tracker;
    }
    const primary = this.selTracker != null ? this.project.tracker(this.selTracker) : null;
    this.selSubject = primary ? primary.subjectId : subject;
    this.sidebar.render();
    this.timeline.invalidate();
    this.viewer.requestDraw();
    this.inspector.requestRender();
    this.tracker.render();
  }

  isSelected(trackerId) {
    return this.selTrackers.has(trackerId);
  }

  selectedTrackers() {
    return [...this.selTrackers].map((id) => this.project.tracker(id)).filter(Boolean);
  }

  // Trackers "Track selected" works on: the selected ones, else the selected subject's.
  trackSelectionIds() {
    if (this.selTrackers.size) return new Set(this.selTrackers);
    return new Set(this.project.trackersOf(this.selSubject).map((p) => p.id));
  }

  selectAll() {
    const pts = this.project.subject(this.selSubject) ? this.project.trackersOf(this.selSubject) : this.project.trackers;
    this.select({ trackers: pts.map((p) => p.id) });
    if (pts.length) toast(`${pts.length} tracker(s) selected — Shift+G tracks just these.`);
  }

  ensureSubject() {
    if (this.project.subject(this.selSubject)) return this.selSubject;
    if (this.project.subjects.length) {
      this.selSubject = this.project.subjects[0].id;
      return this.selSubject;
    }
    let s;
    this.edit("Add subject", () => {
      s = this.project.addSubject();
    });
    this.selSubject = s.id;
    toast(`Created “${s.name}” — double-click its name in the sidebar to rename.`);
    return s.id;
  }

  newSubject() {
    if (!this.ready) return;
    let s;
    this.edit("Add subject", () => {
      s = this.project.addSubject();
    });
    this.select({ subject: s.id, tracker: null });
    this.sidebar.startRename(s.id);
  }

  edit(label, fn) {
    const before = this.project.snapshot();
    fn();
    if (this.project.sameAs(before)) return;
    this.undoStack.push({ label, snap: before });
    if (this.undoStack.length > UNDO_LIMIT) this.undoStack.shift();
    this.redoStack = [];
    this.onModelChanged();
  }

  // Bounds edits: fn(store) changes the subject's bounds store and returns the
  // changed frame range [a, b] (null = nothing changed). Results that depended
  // on the old bounds are truncated from `a`; the undo item keeps them so undo
  // restores them.
  editBounds(label, subjectId, fn) {
    if (this.tracker.active && this.project.trackersOf(subjectId).some((p) => p.kind === "template")) {
      this.tracker.abandon();
      toast("Tracking halted: bounds changed. Press G to continue.");
    }
    const before = this.project.snapshot();
    const range = fn(this.project.ensureBounds(subjectId));
    if (!range) {
      this.project.restore(before);
      return null;
    }
    this.project.boundsChanged();
    this.project.touch(range[0]);
    const invalidate = { kind: "bounds", sid: subjectId, from: range[0] };
    const patch = this.applyInvalidation(invalidate);
    this.undoStack.push({ label, snap: before, patch, invalidate });
    if (this.undoStack.length > UNDO_LIMIT) this.undoStack.shift();
    this.redoStack = [];
    this.onModelChanged();
    if (patch.length) this.onResults();
    this.scheduleQuality();
    return range;
  }

  // A template look change (added, edited, removed, pattern applied) applies
  // from the cursor on: frames before it were tracked with the previous look set
  // and keep their results. One undo step; undo brings the cleared results back.
  editTemplateLook(label, tids, f, fn) {
    if (this.tracker.active) {
      this.tracker.abandon();
      toast("Tracking halted: the template changed. Press G to continue.");
    }
    const before = this.project.snapshot();
    fn();
    if (this.project.sameAs(before)) return;
    const invalidate = { kind: "template", tids, from: f };
    const patch = this.applyInvalidation(invalidate);
    this.undoStack.push({ label, snap: before, patch, invalidate });
    if (this.undoStack.length > UNDO_LIMIT) this.undoStack.shift();
    this.redoStack = [];
    this.onModelChanged();
    if (patch.length) this.onResults();
    this.scheduleQuality();
  }

  applyInvalidation(inv) {
    return inv.kind === "bounds" ? this.invalidateBounds(inv) : this.invalidateTemplates(inv.tids, inv.from);
  }

  // Truncate the given trackers' results from frame f on (patches for undo).
  invalidateTemplates(tids, from) {
    const patches = [];
    for (const id of tids) {
      const p = this.project.tracker(id);
      if (!p) continue;
      for (const seg of this.project.segments(p)) {
        if (seg.end <= from) continue;
        const patch = this.results.truncate(seg.key, Math.max(from, seg.q + 1));
        if (patch) patches.push(patch);
      }
    }
    return patches;
  }

  // Truncate results of the subject's template trackers: their search is
  // restricted to the bounds. CoTracker points keep their results; the bounds
  // only change their starting guess, which measurably matters only for extreme
  // motion (editor/PLAN.md §6.3), so the guide applies from their next run on.
  invalidateBounds({ sid, from }) {
    if (!this.project.subject(sid)) return [];
    const patches = [];
    for (const p of this.project.trackersOf(sid)) {
      if (p.kind !== "template") continue;
      for (const seg of this.project.segments(p)) {
        if (seg.end <= from) continue;
        const patch = this.results.truncate(seg.key, Math.max(from, seg.q + 1));
        if (patch) patches.push(patch);
      }
    }
    return patches;
  }

  undo() {
    const item = this.undoStack.pop();
    if (!item) return;
    if (item.patch?.length) this.tracker.abandon();
    this.redoStack.push({ label: item.label, snap: this.project.snapshot(), invalidate: item.invalidate });
    this.project.restore(item.snap);
    if (item.patch?.length) {
      this.results.restore(item.patch);
      this.onResults();
    }
    this.onModelChanged();
    toast(`Undo: ${item.label}`);
  }

  redo() {
    const item = this.redoStack.pop();
    if (!item) return;
    const undoItem = { label: item.label, snap: this.project.snapshot(), invalidate: item.invalidate };
    this.project.restore(item.snap);
    if (item.invalidate) {
      this.tracker.abandon();
      undoItem.patch = this.applyInvalidation(item.invalidate);
      this.onResults();
    }
    this.undoStack.push(undoItem);
    this.onModelChanged();
    toast(`Redo: ${item.label}`);
  }

  onModelChanged() {
    for (const id of [...this.selTrackers]) if (!this.project.tracker(id)) this.selTrackers.delete(id);
    if (this.selTracker != null && !this.project.tracker(this.selTracker)) this.selTracker = [...this.selTrackers].pop() ?? null;
    if (this.selSubject != null && !this.project.subject(this.selSubject)) this.selSubject = this.project.subjects[0]?.id ?? null;
    $("btn-undo").disabled = !this.undoStack.length;
    $("btn-redo").disabled = !this.redoStack.length;
    this.refreshAll();
    this.scheduleSave();
    this.scheduleQuality();
  }

  refreshAll() {
    this.sidebar.render();
    this.timeline.invalidate();
    this.viewer.requestDraw();
    this.inspector.requestRender();
    this.tracker.render();
    this.updateTransport();
  }

  editFrame() {
    return this.shown >= 0 && !this.player.seeking ? this.shown : this.cursor;
  }

  selectedTracker() {
    return this.selTracker != null ? this.project.tracker(this.selTracker) : null;
  }

  endTracker(id, f) {
    const p = this.project.tracker(id);
    if (!p) return;
    const del = f <= p.start;
    const name = trackerLabel(p);
    this.edit(del ? "Delete tracker" : "End tracker", () => this.project.endTrackerAt(id, f));
    toast(del ? `${name} deleted (it started on this frame).` : `${name} ends at frame ${f}; earlier frames keep it.`);
  }

  endTrackers(pts, f, label) {
    if (!pts.length) return 0;
    const deleted = pts.filter((p) => f <= p.start).length;
    this.edit(label, () => pts.forEach((p) => this.project.endTrackerAt(p.id, f)));
    return deleted;
  }

  endSelectedHere() {
    const pts = this.selectedTrackers();
    if (!pts.length) return;
    if (pts.length === 1) {
      this.endTracker(pts[0].id, this.editFrame());
      return;
    }
    const f = this.editFrame();
    const deleted = this.endTrackers(pts, f, "End trackers");
    toast(`${pts.length} trackers end at frame ${f}${deleted ? ` (${deleted} started here and were deleted)` : ""}.`);
  }

  // Ctrl+drag / Ctrl+Alt+drag: end the selected subject's trackers inside (or
  // outside) a source-space rectangle from this frame on.
  endTrackersInBox(rect, inside) {
    const s = this.project.subject(this.selSubject);
    if (!s) {
      toast("Select a subject first — box-ending only affects the selected subject.", "error");
      return;
    }
    const f = this.editFrame();
    const pts = this.project.trackersOf(s.id).filter((p) => {
      const st = this.project.trackerState(p, f, this.results);
      if (!st || st.x == null) return false;
      const within = st.x >= rect.x0 && st.x <= rect.x1 && st.y >= rect.y0 && st.y <= rect.y1;
      return within === inside;
    });
    if (!pts.length) {
      toast(`No ${s.name} trackers ${inside ? "inside" : "outside"} that box on this frame.`);
      return;
    }
    const deleted = this.endTrackers(pts, f, inside ? "End trackers in box" : "End trackers outside box");
    toast(`${pts.length} ${s.name} tracker(s) ${inside ? "inside" : "outside"} the box end at frame ${f}` +
      `${deleted ? ` (${deleted} started here and were deleted)` : ""}. Ctrl+Z undoes.`);
  }

  // ---- reversible ends -----------------------------------------------------------------

  // Remove the (user or automatic) end from trackers, restoring access to their
  // retained later keys, manual ranges and results. Removing an automatic end
  // also turns automatic ending off for that tracker until re-enabled.
  removeEnds(pts, label = "Remove tracker end") {
    if (!this.meta) return 0;
    const N = this.meta.frameCount;
    const ended = pts.filter((p) => this.project.end(p) < N || p.autoEnd);
    if (!ended.length) {
      toast("Nothing to remove — those trackers already run to the end of the video.");
      return 0;
    }
    const auto = ended.filter((p) => p.autoEnd).length;
    this.edit(label, () => ended.forEach((p) => this.project.clearEnd(p.id)));
    toast(`${ended.length} end(s) removed — later keys and results are available again.` +
      (auto ? ` Automatic ending is now off for ${auto} of them (re-enable it in the tracker panel).` : "") +
      " Press G to compute any missing later frames.");
    this.recomputeQuality();
    return ended.length;
  }

  removeEndSelected() {
    const pts = this.selectedTrackers();
    if (!pts.length) {
      toast("Select a tracker first (click it or its timeline row).");
      return;
    }
    this.removeEnds(pts, pts.length > 1 ? "Remove tracker ends" : "Remove tracker end");
  }

  removeEndsOfSubject(subjectId = this.selSubject) {
    const s = this.project.subject(subjectId);
    if (s) this.removeEnds(this.project.trackersOf(s.id), `Remove ${s.name} ends`);
  }

  // Re-enable automatic ending for a tracker the user overrode.
  setAutoEnd(id, on) {
    this.edit(on ? "Automatic ending on" : "Automatic ending off", () => this.project.setAutoEnd(id, on));
    if (on) this.recomputeQuality();
  }

  // Move the end marker (timeline drag / inspector).
  setTrackerEnd(id, f) {
    const p = this.project.tracker(id);
    if (!p) return;
    const atVideoEnd = f >= this.meta.frameCount;
    this.edit(atVideoEnd ? "Remove tracker end" : "Move tracker end", () => this.project.setTrackerEnd(id, f));
    this.recomputeQuality();
    if (!atVideoEnd) toast(`${trackerLabel(p)} now ends at frame ${f}.`);
  }

  setDriftPolicy(subjectId, policy) {
    this.edit(policy === "end" ? "Automatic ending on" : "Automatic ending off",
      () => this.project.updateSubject(subjectId, { driftPolicy: policy }));
    this.recomputeQuality();
  }

  setMotionPolicy(subjectId, policy) {
    this.edit("Motion policy", () => this.project.updateSubject(subjectId, { motionPolicy: policy }));
    this.recomputeQuality();
  }

  setEscapeSeconds(subjectId, seconds) {
    const v = clamp(Number(seconds) || 0.1, 0.03, 5);
    this.edit("Escape confirmation", () => this.project.updateSubject(subjectId, { escapeSeconds: v }));
    this.recomputeQuality();
  }

  // ---- deterministic quality analysis (automatic ends, motion outliers) -----------------

  scheduleQuality() {
    if (!this.ready || this._qualityTimer) return;
    this._qualityTimer = setTimeout(() => {
      this._qualityTimer = null;
      this.recomputeQuality();
    }, 250);
  }

  recomputeQuality() {
    if (!this.ready) return;
    const changed = [];
    for (const s of this.project.subjects) {
      const watched = (s.driftPolicy ?? "flag") === "end" || (s.motionPolicy ?? "off") !== "off";
      if (!watched) continue;
      for (const p of this.project.recomputeAutoEnds(s.id, this.results)) changed.push(p);
    }
    if (!changed.length) return;
    if (this.tracker.active) {
      const plan = this.tracker.run?.plan || [];
      const entries = [];
      for (const p of changed) {
        const end = this.project.end(p);
        for (const seg of plan) {
          if (seg.trackerId === p.id && seg.end > end) entries.push({ key: seg.key, frame: end });
        }
      }
      if (entries.length) this.tracker.retire(entries);
    }
    this.timeline.invalidate();
    this.viewer.requestDraw();
    this.inspector.requestRender();
    this.sidebar.render();
    this.scheduleSave();
  }

  deleteSelectedTrackers() {
    const pts = this.selectedTrackers();
    if (!pts.length) return;
    this.edit(pts.length > 1 ? "Delete trackers" : "Delete tracker", () => pts.forEach((p) => this.project.deleteTracker(p.id)));
    toast(pts.length > 1 ? `${pts.length} trackers deleted.` : `${trackerLabel(pts[0])} deleted.`);
  }

  // ---- subject offset ----------------------------------------------------------------------

  setOffsetKey(subjectId, f, dx, dy) {
    this.edit("Set offset", () => this.project.setOffsetKey(subjectId, f, dx, dy));
  }

  deleteOffsetKey(subjectId, f) {
    this.edit("Delete offset key", () => this.project.deleteOffsetKey(subjectId, f));
  }

  clearOffset(subjectId) {
    this.edit("Clear offset", () => this.project.clearOffset(subjectId));
  }

  jumpOffsetKey(dir) {
    const s = this.project.subject(this.selSubject);
    if (!s) return;
    const frames = s.offset.map((k) => k.f);
    const f = dir < 0 ? frames.filter((v) => v < this.cursor).pop() : frames.find((v) => v > this.cursor);
    if (f != null) this.seek(f);
  }

  // ---- finetune layers ----------------------------------------------------------------------

  addFinetuneLayer(sid = this.selSubject, opts = {}) {
    const s = this.project.subject(sid);
    if (!s) return null;
    let layer;
    this.edit("Add finetune layer", () => {
      const keys = this.project.newStore(2);
      layer = { id: this.project.nextId++, name: `Finetune ${s.layers.length + 1}`, enabled: true, weight: 1, keys };
      s.layers.push(layer);
      s.activeLayer = layer.id;
      this.project.touch(0);
    });
    if (layer && !opts.silent) {
      toast(`Added “${layer.name}”. In R mode, drag on the frame to nudge it (relative, Shift = 1/10).`);
    }
    return layer;
  }

  setLayer(sid, layerId, patch) {
    const s = this.project.subject(sid);
    const layer = s?.layers.find((l) => l.id === layerId);
    if (!layer) return;
    this.edit("Finetune layer", () => {
      Object.assign(layer, patch);
      this.project.touch(0);
    });
  }

  setActiveLayer(sid, layerId, opts = {}) {
    const s = this.project.subject(sid);
    if (!s || !s.layers.some((l) => l.id === layerId)) return;
    if (s.activeLayer === layerId) return;
    this.edit("Active finetune layer", () => {
      s.activeLayer = layerId;
      this.project.touch(0);
    });
    if (!opts.silent) toast(`Active finetune layer: ${s.layers.find((l) => l.id === layerId).name}.`);
  }

  removeLayer(sid, layerId) {
    const s = this.project.subject(sid);
    const layer = s?.layers.find((l) => l.id === layerId);
    if (!layer) return;
    this.edit("Delete finetune layer", () => {
      s.layers = s.layers.filter((l) => l !== layer);
      this.project.stores.delete(String(layer.keys));
      if (s.activeLayer === layerId) s.activeLayer = s.layers[0]?.id ?? null;
      this.project.touch(0);
    });
    toast(`Deleted “${layer.name}”.`);
  }

  clearLayer(sid, layerId) {
    const s = this.project.subject(sid);
    const layer = s?.layers.find((l) => l.id === layerId);
    const store = layer && this.project.store(layer.keys);
    if (!store || store.empty) return;
    this.edit("Clear finetune layer", () => {
      store.clearRange(0, this.meta.frameCount);
      this.project.touch(0);
    });
    toast(`Cleared “${layer.name}”.`);
  }

  // ---- bounds ---------------------------------------------------------------------------

  clearBounds(subjectId, from = 0) {
    const s = this.project.subject(subjectId);
    if (!this.project.boundsStore(s) && !(s?.boundsPasses || []).length) return;
    const range = this.editBounds(from ? "Clear bounds from here" : "Clear bounds", subjectId, (store) => {
      const cleared = clearBounds(store, from, this.meta.frameCount - 1);
      const trimmed = this.project.trimBoundsPasses(subjectId, from);
      if (cleared == null && trimmed == null) return null;
      return [Math.min(cleared?.[0] ?? Infinity, trimmed ?? Infinity), this.meta.frameCount - 1];
    });
    if (range) toast(`${s.name}: bounds cleared on frames ${range[0]}–${range[1]}.`);
  }

  // ---- bounds passes ---------------------------------------------------------------------

  // Record a Puppeteer pass (or import). Undoable; template results that
  // depended on the bounds are truncated from the pass start.
  addBoundsPass(subjectId, pass) {
    const s = this.project.subject(subjectId);
    if (!s) return null;
    if (this.tracker.active && this.project.trackersOf(subjectId).some((p) => p.kind === "template")) {
      this.tracker.abandon();
      toast("Tracking halted: bounds changed. Press G to continue.");
    }
    const before = this.project.snapshot();
    const rec = this.project.addBoundsPass(subjectId, pass);
    if (!rec) return null;
    const invalidate = { kind: "bounds", sid: subjectId, from: Math.max(0, rec.a ?? 0) };
    const patch = this.applyInvalidation(invalidate);
    this.undoStack.push({ label: "Puppeteer pass", snap: before, patch, invalidate });
    if (this.undoStack.length > UNDO_LIMIT) this.undoStack.shift();
    this.redoStack = [];
    this.onModelChanged();
    if (patch.length) this.onResults();
    this.scheduleQuality();
    return rec;
  }

  // Change a pass's enabled/weight/name/settings/range. Settings or range
  // changes regenerate its boxes from the stored pointer samples.
  editBoundsPass(label, subjectId, passId, patch) {
    const pass = this.project.boundsPass(subjectId, passId);
    if (!pass) return null;
    if (this.tracker.active && this.project.trackersOf(subjectId).some((p) => p.kind === "template")) {
      this.tracker.abandon();
      toast("Tracking halted: bounds changed. Press G to continue.");
    }
    const before = this.project.snapshot();
    const settings = patch.settings ? { ...pass.settings, ...patch.settings } : null;
    const from = Math.max(0, Math.min(pass.a ?? 0, patch.a ?? pass.a ?? 0));
    this.project.updateBoundsPass(subjectId, passId, settings ? { ...patch, settings } : patch);
    if ((settings || patch.a != null || patch.b != null) && (pass.samples || []).length) {
      const updated = this.project.boundsPass(subjectId, passId);
      this.project.setBoundsPassBoxes(subjectId, passId, updated.a, regeneratePass(this, updated));
    }
    const invalidate = { kind: "bounds", sid: subjectId, from };
    const resultPatch = this.applyInvalidation(invalidate);
    this.undoStack.push({ label, snap: before, patch: resultPatch, invalidate });
    if (this.undoStack.length > UNDO_LIMIT) this.undoStack.shift();
    this.redoStack = [];
    this.onModelChanged();
    if (resultPatch.length) this.onResults();
    this.scheduleQuality();
    return this.project.boundsPass(subjectId, passId);
  }

  removeBoundsPass(subjectId, passId) {
    const pass = this.project.boundsPass(subjectId, passId);
    if (!pass) return;
    this.editBounds("Delete bounds pass", subjectId, (store) => {
      this.project.removeBoundsPass(subjectId, passId);
      return [pass.a ?? 0, pass.b ?? this.meta.frameCount - 1];
    });
  }

  setBoundsGuide(subjectId, on) {
    this.edit(on ? "Bounds guide on" : "Bounds guide off", () => this.project.updateSubject(subjectId, { boundsGuide: on }));
  }

  driftedHere(subjectId) {
    const f = this.editFrame();
    return this.project.trackersOf(subjectId).filter((p) => this.project.trackerState(p, f, this.results)?.drifted);
  }

  // End each drifted tracker where its current drifted run began.
  endDriftedHere(subjectId) {
    const f = this.editFrame();
    const plans = this.driftedHere(subjectId).map((p) => [p, this.project.driftedRunStart(p, f, this.results)]);
    if (!plans.length) return;
    const deleted = plans.filter(([p, g]) => g <= p.start).length;
    this.edit("End drifted trackers", () => plans.forEach(([p, g]) => this.project.endTrackerAt(p.id, g)));
    toast(`${plans.length} drifted tracker(s) end where they left the bounds` +
      `${deleted ? ` (${deleted} were outside from their first frame and were deleted)` : ""}. Ctrl+Z undoes.`);
  }

  setMode(name) {
    const v = this.viewer;
    const target = name === "bounds" ? v.boundsMode : name === "puppeteer" ? v.puppeteer
      : name === "finetune" ? v.finetune : v.normal;
    if (target !== v.normal && !this.project.subject(this.selSubject)) {
      toast("Select a subject first (click it in the sidebar or press 1–9).", "error");
      return;
    }
    v.setMode(v.mode === target ? v.normal : target);
  }

  onModeChanged() {
    document.body.dataset.mode = this.viewer.mode.name;
    this.sidebar.render();
  }

  openTemplateEditor(rect) {
    const p = this.selTrackers.size === 1 ? this.selectedTracker() : null;
    this.templateEditor.open(rect, p?.kind === "template" ? p : null);
  }

  // Open the popup on an existing look (mask/hotspot editing).
  openLookEditor(id) {
    const p = this.selectedTracker();
    const look = p?.looks?.find((l) => l.id === id);
    if (look) this.templateEditor.open(null, p, look);
  }

  // ---- pattern library ---------------------------------------------------------------------

  // Use a saved pattern: on a selected template tracker it becomes another look
  // from the cursor on (pixels/mask/hotspot copied into the project, so later
  // library edits don't change it); otherwise the editor opens on the pattern so
  // a new tracker's hotspot can be placed explicitly.
  usePattern(id) {
    const pattern = patternById(id);
    if (!pattern) {
      toast("That pattern no longer exists.", "error");
      return;
    }
    const p = this.selTrackers.size === 1 ? this.selectedTracker() : null;
    if (p?.kind === "template") {
      const f = this.editFrame();
      const st = this.project.trackerState(p, f, this.results);
      const cx = st?.x ?? this.meta.width / 2;
      const cy = st?.y ?? this.meta.height / 2;
      const look = { f, w: pattern.w, h: pattern.h, hx: pattern.hx, hy: pattern.hy, mask: pattern.mask || "",
        tmpl: pattern.tmpl, pid: pattern.id, x: Math.round(cx - pattern.hx), y: Math.round(cy - pattern.hy) };
      this.editTemplateLook("Use pattern", [p.id], f, () => this.project.addLook(p.id, look));
      toast(`“${pattern.name}” added to ${trackerLabel(p)} from frame ${f}. Press G to track.`);
      return;
    }
    this.templateEditor.openPattern(pattern, null);
  }

  exportPatternLibrary() {
    download("cotrack-patterns.json", exportPatterns(), "application/json");
    toast("Pattern library exported");
  }

  importPatternLibrary() {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = ".json,application/json";
    input.onchange = async () => {
      const file = input.files?.[0];
      if (!file) return;
      try {
        const { added } = importPatterns(await file.text());
        this.sidebar.renderPatterns();
        toast(`Imported ${added} pattern(s) — they're in the Patterns list.`);
      } catch (err) {
        toast(err.message, "error");
      }
    };
    input.click();
  }

  // Clear a tracker's template results from frame f on, keeping everything
  // before it (the looks that produced those frames haven't changed). Not
  // undoable on its own — results can be regenerated.
  truncateTemplatesFrom(pts, f) {
    const n = this.invalidateTemplates(pts.map((p) => p.id), f).length;
    if (n) this.onResults();
    return n;
  }

  retrackSelectedHere() {
    const pts = this.selectedTrackers().filter((p) => p.kind === "template");
    if (!pts.length) return;
    if (this.tracker.active) {
      toast("Halt tracking before re-tracking a template.");
      return;
    }
    const f = this.editFrame();
    const n = this.truncateTemplatesFrom(pts, f);
    toast(n
      ? `Cleared ${pts.length} template tracker(s) from frame ${f}; press G to re-track.`
      : `Nothing to clear from frame ${f} — the templates are already re-tracked from there.`);
  }

  // Toggle manual animation for every selected tracker on this frame. Auto
  // tracking can't resume from off-frame, so those trackers stay manual.
  toggleManual() {
    const pts = this.selectedTrackers();
    if (!pts.length) {
      toast("Select a tracker first (click it).");
      return;
    }
    const f = this.editFrame();
    const plans = [];
    const skipped = [];
    for (const p of pts) {
      const st = this.project.trackerState(p, f, this.results);
      if (!st) {
        skipped.push(`${trackerLabel(p)} (not on this frame)`);
        continue;
      }
      const inManual = this.project.manualRangeAt(p, f);
      if (inManual && st.x != null && !this.project.isInside(st.x, st.y)) {
        skipped.push(`${trackerLabel(p)} (off-frame — move it inside the frame to resume auto tracking)`);
        continue;
      }
      const pos = st.x != null ? { x: st.x, y: st.y } : null;
      if (!pos && !inManual) {
        skipped.push(`${trackerLabel(p)} (no known position yet)`);
        continue;
      }
      plans.push({ p, pos });
    }
    const results = [];
    if (plans.length) {
      this.edit("Toggle manual", () => {
        for (const { p, pos } of plans) results.push([trackerLabel(p), this.project.toggleManualAt(p.id, f, pos)]);
      });
    }
    if (skipped.length) toast(`Skipped ${skipped.join(", ")}.`, plans.length ? "" : "error");
    if (results.length === 1) {
      const [id, mode] = results[0];
      toast(mode === "manual"
        ? `${id}: manual animation from frame ${f}. Drag it on any frame to key it (even outside the frame); M again to stop.`
        : `${id}: auto tracking resumes at frame ${f}. Press G to track.`);
    } else if (results.length > 1) {
      const nManual = results.filter(([, m]) => m === "manual").length;
      toast(`Frame ${f}: ${nManual} tracker(s) now manual, ${results.length - nManual} back to auto.`);
    }
  }

  goSelected() {
    const ids = this.trackSelectionIds();
    if (!ids.size) {
      toast("Select trackers first (Ctrl+click, or drag on the frame to box-select), or select a subject.");
      return;
    }
    this.tracker.go(ids);
  }

  jumpKey(dir) {
    const p = this.selectedTracker();
    if (!p) {
      this.jumpOffsetKey(dir);
      return;
    }
    const frames = [...new Set([p.start, ...p.keys.map((k) => k.f), ...p.manual.flatMap((r) => [r.a, r.b ?? p.start]), p.end ?? p.start])]
      .filter((f) => f < this.meta.frameCount)
      .sort((a, b) => a - b);
    const f = dir < 0 ? frames.filter((v) => v < this.cursor).pop() : frames.find((v) => v > this.cursor);
    if (f != null) this.seek(f);
  }

  onResults() {
    if (this._resultsRaf) return;
    this._resultsRaf = requestAnimationFrame(() => {
      this._resultsRaf = 0;
      this.timeline.invalidate();
      this.viewer.requestDraw();
      this.scheduleStatusRefresh();
      this.scheduleSave();
      this.scheduleQuality();
    });
  }

  onTrackerChanged() {
    this.timeline.invalidate();
    if (!this.tracker.active && this.dirty) this.scheduleSave(true);
  }

  // ---- persistence ------------------------------------------------------------------------

  setSaveStatus(text) {
    $("save-status").textContent = text;
  }

  scheduleSave(soon = false) {
    if (!this.ready) return;
    this.dirty = true;
    this.setSaveStatus("Unsaved");
    if (this.tracker.active && !soon) {
      if (!this._saveTimer) this._saveTimer = setTimeout(() => this.save(), 20000);
      return;
    }
    clearTimeout(this._saveTimer);
    this._saveTimer = setTimeout(() => this.save(), soon ? 300 : 1500);
  }

  async save() {
    clearTimeout(this._saveTimer);
    this._saveTimer = null;
    if (!this.ready || !this.meta) return;
    if (this._saving) {
      this._saveAgain = true;
      return this._saving;
    }
    const meta = this.meta;
    const payload = JSON.stringify({
      format: "cotrack.project",
      version: 2,
      videoId: meta.id,
      savedAt: new Date().toISOString(),
      ui: { cursor: this.cursor, selSubject: this.selSubject },
      project: this.project.toJSON(),
      results: this.results.serialize(this.project.resultKeys(this.results)),
    });
    this.dirty = false;
    this.setSaveStatus("Saving…");
    this._saving = api
      .saveProject(meta.id, payload)
      .then(() => this.meta === meta && !this.dirty && this.setSaveStatus("Saved"))
      .catch((err) => {
        this.dirty = true;
        this.setSaveStatus("Save failed");
        toast(`Save failed: ${err.message}`, "error");
      })
      .finally(() => {
        this._saving = null;
        if (this._saveAgain) {
          this._saveAgain = false;
          this.save();
        }
      });
    return this._saving;
  }

  // ---- UI wiring ----------------------------------------------------------------------------

  bindUI() {
    $("btn-choose").addEventListener("click", () => $("file-input").click());
    $("drop-zone").addEventListener("click", (e) => {
      if (e.target === $("drop-zone") || e.target.closest(".drop-icon")) $("file-input").click();
    });
    $("file-input").addEventListener("change", (e) => {
      this.openFile(e.target.files[0]);
      e.target.value = "";
    });
    $("path-form").addEventListener("submit", (e) => {
      e.preventDefault();
      const path = $("path-input").value.trim();
      if (path) this.openPath(path);
    });
    $("recent").addEventListener("click", (e) => {
      const el = e.target.closest(".recent-item");
      const meta = el && this._recent?.find((v) => v.id === el.dataset.id);
      if (meta) this.openMeta(meta);
    });

    $("btn-undo").addEventListener("click", () => this.undo());
    $("btn-redo").addEventListener("click", () => this.redo());
    $("btn-undo").disabled = $("btn-redo").disabled = true;
    $("btn-export-json").addEventListener("click", () => exportJSON(this));
    $("btn-export-csv").addEventListener("click", () => exportCSV(this));
    $("btn-export-norm").addEventListener("click", () => exportNormalizedCSV(this));
    $("btn-export-fusion").addEventListener("click", () => exportFusion(this));
    $("btn-help").addEventListener("click", () => this.toggleHelp(true));
    $("btn-help-close").addEventListener("click", () => this.toggleHelp(false));
    $("help-overlay").addEventListener("click", (e) => e.target === $("help-overlay") && this.toggleHelp(false));
    $("btn-close").addEventListener("click", () => this.closeVideo(true));
    $("btn-source").addEventListener("click", () => this.toggleSource());

    $("transport").addEventListener("click", (e) => {
      const act = e.target.closest("[data-act]")?.dataset.act;
      if (!act || !this.ready) return;
      const actions = {
        first: () => this.seek(0),
        prev: () => this.seek(this.cursor - 1),
        play: () => this.player.toggle(),
        next: () => this.seek(this.cursor + 1),
        last: () => this.seek(this.meta.frameCount - 1),
        mute: () => {
          this.video.muted = !this.video.muted;
          $("btn-mute").textContent = this.video.muted ? "🔇" : "🔊";
        },
        fit: () => this.timeline.reset(),
      };
      actions[act]?.();
      e.target.closest("button")?.blur();
    });
    $("frame-input").addEventListener("change", (e) => this.seek(Number(e.target.value) || 0));
    $("frame-input").addEventListener("keydown", (e) => {
      if (e.key === "Enter") e.target.blur();
    });

    this.video.addEventListener("play", () => this.updateTransport());
    this.video.addEventListener("pause", () => this.updateTransport());
    this.video.addEventListener("error", () => {
      if (!this.video.getAttribute("src")) return;
      const el = $("video-error");
      el.classList.remove("hidden");
      const building = this.meta?.needsProxy && this.meta.proxy?.state !== "ready" && this.meta.proxy?.state !== "error";
      el.innerHTML = `<b>This browser can't play this video${this.meta ? ` (codec: ${escapeHtml(this.meta.codec)})` : ""}.</b><br>
        ${building
          ? "A browser-friendly proxy is being built on your GPU and will appear here automatically when it's ready."
          : "Chrome/Edge play H.264, VP9 and AV1 (HEVC depends on your system). Re-encode to H.264 (MP4 or MKV) to edit it here."}`;
    });
    window.addEventListener("beforeunload", (e) => {
      if (this.ready && this.dirty) {
        this.save();
        e.preventDefault();
      }
    });
  }

  toggleHelp(show) {
    $("help-overlay").classList.toggle("hidden", !show);
  }

  bindKeys() {
    let spaceUsedForPan = false;
    window.addEventListener("keydown", (e) => {
      if (this.templateEditor.active) return;
      const t = e.target;
      if (t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement || t.isContentEditable) {
        if (e.key === "Escape") t.blur();
        return;
      }
      if (!$("help-overlay").classList.contains("hidden")) {
        if (e.key === "Escape" || e.key === "?") this.toggleHelp(false);
        return;
      }
      if (e.key === "?") {
        this.toggleHelp(true);
        return;
      }
      if (!this.ready) return;
      if (this.viewer.mode.onKey(e)) {
        e.preventDefault();
        return;
      }
      const k = e.key;
      const mod = e.ctrlKey || e.metaKey;
      if (mod) {
        const lk = k.toLowerCase();
        if (lk === "z") this.redoOrUndo(e.shiftKey);
        else if (lk === "y") this.redo();
        else if (lk === "s") this.save();
        else if (lk === "a") this.selectAll();
        else return;
        e.preventDefault();
        return;
      }
      if (t instanceof HTMLButtonElement && (k === " " || k === "Enter")) t.blur();
      const step = e.shiftKey ? 10 : e.altKey ? Math.round(this.meta.fps) : 1;
      switch (k) {
        case "ArrowLeft":
          this.seek(this.cursor - step);
          break;
        case "ArrowRight":
          this.seek(this.cursor + step);
          break;
        case "Home":
          this.seek(0);
          break;
        case "End":
          this.seek(this.meta.frameCount - 1);
          break;
        case " ":
          if (!e.repeat) {
            this.viewer.spaceDown = true;
            spaceUsedForPan = false;
          }
          break;
        case "g":
        case "G":
          if (this.tracker.active) this.tracker.halt();
          else if (e.shiftKey) this.goSelected();
          else this.tracker.go();
          break;
        case "n":
        case "N":
          this.newSubject();
          break;
        case "m":
        case "M":
          this.toggleManual();
          break;
        case "e":
        case "E":
          this.endSelectedHere();
          break;
        case "u":
        case "U":
          this.removeEndSelected();
          break;
        case "b":
        case "B":
          this.setMode("bounds");
          break;
        case "p":
        case "P":
          this.setMode("puppeteer");
          break;
        case "r":
        case "R":
          this.setMode("finetune");
          break;
        case "Delete":
        case "Backspace":
          if (this.endMarkerFocus != null && this.selTrackers.has(this.endMarkerFocus)) {
            this.removeEnds([this.project.tracker(this.endMarkerFocus)], "Remove tracker end");
          } else this.deleteSelectedTrackers();
          break;
        case "Escape":
          this.select({ subject: this.selSubject, tracker: null });
          break;
        case "a":
        case "A":
          this.selectAll();
          break;
        case "[":
          this.jumpKey(-1);
          break;
        case "]":
          this.jumpKey(1);
          break;
        case "f":
        case "F":
          this.viewer.fit();
          break;
        case "\\":
          this.timeline.reset();
          break;
        default:
          if (/^[1-9]$/.test(k)) {
            const s = this.project.subjects[Number(k) - 1];
            if (s) this.select({ subject: s.id, tracker: null });
            break;
          }
          return;
      }
      e.preventDefault();
    });
    window.addEventListener("keyup", (e) => {
      if (e.key !== " ") return;
      const wasDown = this.viewer.spaceDown;
      this.viewer.spaceDown = false;
      if (wasDown && !spaceUsedForPan && this.ready) this.player.toggle();
    });
    this.viewer.canvas.addEventListener("pointerdown", () => {
      if (this.viewer.spaceDown) spaceUsedForPan = true;
    }, true);
  }

  redoOrUndo(redo) {
    if (redo) this.redo();
    else this.undo();
  }

  bindDrop() {
    let depth = 0;
    const overlay = $("drag-overlay");
    const hasFiles = (e) => [...(e.dataTransfer?.types || [])].includes("Files");
    window.addEventListener("dragenter", (e) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth++;
      overlay.classList.remove("hidden");
    });
    window.addEventListener("dragover", (e) => {
      if (hasFiles(e)) e.preventDefault();
    });
    window.addEventListener("dragleave", (e) => {
      if (!hasFiles(e)) return;
      depth = Math.max(0, depth - 1);
      if (!depth) overlay.classList.add("hidden");
    });
    window.addEventListener("drop", (e) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth = 0;
      overlay.classList.add("hidden");
      this.openFile(e.dataTransfer.files[0]);
    });
  }
}

window.app = new App();
