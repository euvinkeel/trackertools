import { duration, fitCanvas, toast } from "./util.js";

// Client for the background tracking job. Runs are manual (Go / Halt) and
// stream results + preview frames back over a WebSocket while you keep editing.
export class Tracker {
  constructor(app) {
    this.app = app;
    this.ws = null;
    this.runId = 0;
    this.state = "idle";
    this.status = {};
    this.run = null;
    this.previewImg = new Image();
    this.preview = null;
    this.els = {
      go: document.getElementById("btn-go"),
      goSel: document.getElementById("btn-go-sel"),
      pill: document.getElementById("tracker-state"),
      bar: document.getElementById("tracker-progress"),
      stats: document.getElementById("tracker-stats"),
      canvas: document.getElementById("tracker-preview"),
      label: document.getElementById("preview-label"),
      jump: document.getElementById("btn-jump-tracker"),
    };
    this.els.go.addEventListener("click", () => this.toggle());
    this.els.goSel.addEventListener("click", () => app.goSelected());
    this.els.jump.addEventListener("click", () => {
      if (this.preview) app.seek(this.preview.frame);
    });
    this.previewImg.onload = () => this.drawPreview();
    new ResizeObserver(() => this.drawPreview()).observe(this.els.canvas);
    this.connect();
  }

  get active() {
    return this.state === "starting" || this.state === "running";
  }

  connect() {
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${location.host}/api/track`);
    ws.onopen = () => {
      this.ws = ws;
      this.render();
    };
    ws.onmessage = (e) => this.onMessage(JSON.parse(e.data));
    ws.onclose = () => {
      this.ws = null;
      if (this.active) {
        this.state = "error";
        this.status = { ...this.status, message: "Lost connection to the server." };
        this.render();
      }
      setTimeout(() => this.connect(), 1000);
    };
  }

  reset() {
    this.runId++;
    this.state = "idle";
    this.status = {};
    this.run = null;
    this.preview = null;
    this.render();
    this.drawPreview();
  }

  toggle() {
    if (this.active) this.halt();
    else this.go();
  }

  // Track from the cursor. onlyTrackerIds limits the run to a subset of trackers;
  // by default every tracker that needs it catches up.
  go(onlyTrackerIds = null) {
    const app = this.app;
    if (!app.ready || this.active) return;
    const start = app.cursor;
    const { segments, blocked, bounds } = app.project.planRun(start, app.results, onlyTrackerIds);
    const blockedMsg = blocked
      ? ` ${blocked} segment(s) resume from off-frame and can't be auto-tracked — keep them manual or drag them inside the frame.`
      : "";
    if (!segments.length) {
      const scope = onlyTrackerIds ? "the selected trackers" : "every tracker";
      toast((app.project.trackers.length
        ? `Nothing to track from here — ${scope} after the cursor is already tracked or manual.`
        : "Add points first: select a subject and click on the video.") + blockedMsg);
      return;
    }
    if (blocked) toast(blockedMsg.trim());
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      toast("Not connected to the tracking server.", "error");
      return;
    }
    this.runId++;
    const N = app.meta.frameCount;
    const first = Math.min(...segments.map((s) => s.q));
    const plannedEnd = Math.max(...segments.map((s) => s.end ?? N));
    this.run = {
      runId: this.runId, startFrame: start, first, plannedEnd, segments: segments.length, plan: segments,
      subset: onlyTrackerIds ? new Set(segments.map((s) => s.trackerId)).size : null,
      t0: performance.now(),
    };
    this.state = "starting";
    this.status = { frame: first };
    this.ws.send(JSON.stringify({ type: "start", runId: this.runId, videoId: app.meta.id, startFrame: start, segments, bounds }));
    if (first < start) toast(`Starting at frame ${first}: some trackers need tracking before the cursor to get here.`);
    this.render();
    app.onTrackerChanged();
  }

  halt() {
    if (this.ws && this.active) this.ws.send(JSON.stringify({ type: "halt" }));
  }

  // Ask a running job to stop tracking the given segments at a frame (used when
  // quality analysis confirms an automatic end). Already-computed results are
  // archived; the effective cutoff keeps them from contributing.
  retire(entries) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const list = entries.filter((e) => e && e.key);
    if (!list.length) return;
    this.ws.send(JSON.stringify({ type: "retire", runId: this.runId, entries: list }));
  }

  // Halt and ignore anything the run still sends: its results are stale
  // (e.g. the bounds it was searching in just changed).
  abandon() {
    if (!this.active) return;
    this.halt();
    this.runId++;
    this.state = "halted";
    this.render();
    this.app.onTrackerChanged();
  }

  onMessage(m) {
    if (m.runId !== this.runId) return;
    const app = this.app;
    if (m.type === "results") {
      for (const [key, it] of Object.entries(m.items)) app.results.write(key, it.q, it.f0, it.data);
      app.onResults();
    } else if (m.type === "status") {
      this.status = { ...this.status, ...m };
      this.state = m.state;
      if (m.state === "running" && this.run && m.startFrame != null) this.run.first = m.startFrame;
      if (m.state === "done") toast("Tracking finished.");
      if (m.state === "error") toast(m.message || "Tracking failed", "error");
      if (m.state === "loading") toast(m.message || "Model is loading…");
      this.render();
      app.onTrackerChanged();
    } else if (m.type === "preview") {
      this.preview = m;
      this.previewImg.src = m.image;
    }
  }

  render() {
    const { els, run, status } = this;
    const active = this.active;
    const app = this.app;
    els.go.disabled = !app.ready;
    els.go.innerHTML = active ? "■ Halt <kbd>G</kbd>" : "▶ Go from cursor <kbd>G</kbd>";
    els.go.classList.toggle("halt", active);
    const nSel = app.ready ? app.trackSelectionIds().size : 0;
    els.goSel.disabled = active || !nSel;
    els.goSel.innerHTML = `▶ Selected${nSel ? ` (${nSel})` : ""} <kbd>⇧G</kbd>`;
    els.pill.textContent = this.ws ? this.state : "offline";
    els.pill.className = `pill ${this.state}`;
    els.jump.disabled = !this.preview;
    if (!run) {
      els.bar.style.width = "0";
      els.stats.textContent = this.ws
        ? "Place trackers, then press Go. Tracking runs in the background while you keep working."
        : "Connecting to the tracking server…";
      return;
    }
    const frame = status.frame ?? run.first;
    const span = Math.max(1, run.plannedEnd - run.first);
    els.bar.style.width = `${Math.min(100, ((frame - run.first) / span) * 100)}%`;
    const elapsed = (performance.now() - run.t0) / 1000;
    const fps = status.fps;
    const lines = [];
    if (this.state === "running" || this.state === "starting") {
      lines.push(`Frame ${frame.toLocaleString()} / ${run.plannedEnd.toLocaleString()}${fps ? ` · ${fps} fps` : ""}`);
      const scope = run.subset != null ? `${run.subset} selected tracker(s)` : `${run.segments} segment(s)`;
      lines.push(fps ? `ETA ${duration((run.plannedEnd - frame) / fps)} · ${scope}` : `Starting ${scope}…`);
    } else if (this.state === "done") {
      lines.push(`Finished at frame ${frame.toLocaleString()} in ${duration(elapsed)}.`);
      lines.push("Scrub to review; correct anything and press Go again.");
    } else if (this.state === "halted") {
      lines.push(`Halted at frame ${frame.toLocaleString()}.`);
      lines.push("Fix what you need, then Go continues from the cursor.");
    } else if (this.state === "error" || this.state === "loading") {
      lines.push(status.message || this.state);
    }
    els.stats.innerHTML = lines.join("<br>");
  }

  drawPreview() {
    const { ctx, w, h } = fitCanvas(this.els.canvas);
    ctx.fillStyle = "#000";
    ctx.fillRect(0, 0, w, h);
    const m = this.preview;
    const app = this.app;
    if (!m || !this.previewImg.complete || !this.previewImg.naturalWidth || !app.meta) {
      this.els.label.textContent = "";
      return;
    }
    const iw = this.previewImg.naturalWidth;
    const ih = this.previewImg.naturalHeight;
    const s = Math.min(w / iw, h / ih);
    const dw = iw * s;
    const dh = ih * s;
    const dx = (w - dw) / 2;
    const dy = (h - dh) / 2;
    ctx.drawImage(this.previewImg, dx, dy, dw, dh);
    const index = app.project.segmentIndex();
    for (const [key, x, y, vis] of m.points) {
      const seg = index.get(key);
      const subj = seg ? app.project.subject(seg.subjectId) : null;
      const px = dx + (x / app.meta.width) * dw;
      const py = dy + (y / app.meta.height) * dh;
      ctx.beginPath();
      if (seg?.kind === "template") ctx.rect(px - 3.5, py - 3.5, 7, 7);
      else ctx.arc(px, py, 3.5, 0, Math.PI * 2);
      ctx.fillStyle = vis >= 0.6 ? subj?.color || "#fff" : "rgba(0,0,0,.4)";
      ctx.fill();
      ctx.strokeStyle = vis >= 0.6 ? "#fff" : subj?.color || "#fff";
      ctx.lineWidth = 1.25;
      if (seg?.kind === "template" && vis < 0.6) ctx.setLineDash([2, 2]);
      ctx.stroke();
      ctx.setLineDash([]);
    }
    this.els.label.textContent = `tracking · frame ${m.frame.toLocaleString()}`;
  }
}
