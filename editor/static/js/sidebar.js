import { trackerLabel, VIS_THRESHOLD } from "./project.js";
import { api } from "./api.js";
import { boundsFalloffSeconds, setBoundsFalloffSeconds } from "./modes.js";
import { deletePattern, isPersistent, listPatterns } from "./patterns.js";
import { puppeteerSettings, savePuppeteerSettings, SPEEDS, PRESETS, presetValues } from "./puppeteer.js";
import { escapeHtml, fmt } from "./util.js";

export function trackerModeLabel(st) {
  if (!st) return ["not here", ""];
  if (st.lost) return ["not found", "lost"];
  if (st.drifted) return ["drifted", "drifted"];
  if (st.mode === "auto") return st.vis >= VIS_THRESHOLD ? ["tracked", "auto"] : ["hidden", "hidden-b"];
  if (st.mode === "manual") return ["manual", "manual"];
  if (st.mode === "pending") return ["needs tracking", "pending"];
  if (st.mode === "blocked") return ["off-frame seed", "blocked"];
  return ["no data", ""];
}

const xy = (x, y) => `${fmt(x)}, ${fmt(y)}`;
const speeds = (current) => SPEEDS.map((r) => `<option value="${r}" ${r === current ? "selected" : ""}>${r}×</option>`).join("");

export class Sidebar {
  constructor(app) {
    this.app = app;
    this.listEl = document.getElementById("subject-list");
    this.panelEl = document.getElementById("point-panel");
    this.patternEl = document.getElementById("pattern-panel");
    this.renaming = null;
    document.getElementById("btn-add-subject").addEventListener("click", () => app.newSubject());
    this.patternEl.addEventListener("click", (e) => {
      const act = e.target.closest("[data-act]")?.dataset.act;
      const id = e.target.closest("[data-pat]")?.dataset.pat;
      if (act === "use-pattern" && id) app.usePattern(id);
      else if (act === "del-pattern" && id) {
        const p = listPatterns().find((x) => x.id === id);
        if (p && confirm(`Delete the pattern “${p.name}”? Trackers already using it keep their copied pixels.`)) {
          deletePattern(id);
          this.renderPatterns();
        }
      } else if (act === "export-patterns") app.exportPatternLibrary();
      else if (act === "import-patterns") app.importPatternLibrary();
      if (act) e.target.closest("button")?.blur();
    });
    this.listEl.addEventListener("click", (e) => this.onListClick(e));
    this.listEl.addEventListener("dblclick", (e) => {
      const el = e.target.closest(".subject");
      if (el && e.target.closest(".name")) this.startRename(Number(el.dataset.sid));
    });
    this.panelEl.addEventListener("click", (e) => this.onPanelClick(e));
    this.panelEl.addEventListener("change", (e) => {
      const act = e.target.dataset.act;
      const value = Number(e.target.value);
      if (act === "threshold") {
        const p = app.selectedTracker();
        if (p?.kind === "template") app.edit("Set match threshold", () => app.project.setThreshold(p.id, value));
      } else if (act === "pp-rate") savePuppeteerSettings({ rate: value });
      else if (act === "pp-gain") savePuppeteerSettings({ gain: value });
      else if (act === "pp-lag") savePuppeteerSettings({ lag: value });
      else if (act === "pp-minhalf") savePuppeteerSettings({ minHalf: value });
      else if (act === "pp-pad") savePuppeteerSettings({ pad: value });
      else if (act === "pp-preset") {
        savePuppeteerSettings({ preset: e.target.value, ...presetValues(e.target.value) });
        this.render();
      } else if (act === "pass-enabled" && app.selSubject != null) {
        app.editBoundsPass("Toggle bounds pass", app.selSubject, Number(e.target.dataset.pass), { enabled: e.target.checked });
      } else if (act === "pass-weight" && app.selSubject != null) {
        app.editBoundsPass("Pass weight", app.selSubject, Number(e.target.dataset.pass), { weight: value });
      } else if (["pass-lag", "pass-gain", "pass-minhalf", "pass-pad"].includes(act) && app.selSubject != null) {
        const key = { "pass-lag": "lag", "pass-gain": "gain", "pass-minhalf": "minHalf", "pass-pad": "pad" }[act];
        app.editBoundsPass("Tune bounds pass", app.selSubject, Number(e.target.dataset.pass), { settings: { [key]: value } });
      } else if (act === "falloff" && value > 0) {
        setBoundsFalloffSeconds(value);
        this.renderStatus();
      } else if (act === "bounds-guide" && app.selSubject != null) app.setBoundsGuide(app.selSubject, e.target.checked);
      else if (act === "auto-end" && app.selTracker != null) app.setAutoEnd(app.selTracker, e.target.checked);
      else if (act === "drift-end" && app.selSubject != null) app.setDriftPolicy(app.selSubject, e.target.checked ? "end" : "flag");
      else if (act === "escape-seconds" && app.selSubject != null) app.setEscapeSeconds(app.selSubject, value);
      else if (act === "motion-policy" && app.selSubject != null) app.setMotionPolicy(app.selSubject, e.target.value);
      else if (act === "layer-enabled" && app.selSubject != null) {
        app.setLayer(app.selSubject, Number(e.target.dataset.layer), { enabled: e.target.checked });
      } else if (act === "layer-weight" && app.selSubject != null) {
        app.setLayer(app.selSubject, Number(e.target.dataset.layer), { weight: value });
      }
      if (act?.startsWith("pp-") || act === "falloff") app.viewer.requestDraw();
    });
    this.panelEl.addEventListener("input", (e) => {
      if (e.target.matches('[data-act="threshold"]')) this.panelEl.querySelector(".threshold-value").textContent = Number(e.target.value).toFixed(2);
    });
  }

  frame() {
    return this.app.shown >= 0 ? this.app.shown : this.app.cursor;
  }

  render() {
    const app = this.app;
    const { project } = app;
    if (!project.subjects.length) {
      this.listEl.innerHTML = `<div class="empty-hint">No subjects yet.<br>Press <kbd>N</kbd> or <b>+ Subject</b>, name it,
        then click on the video to place its points. Trackers carry each subject: they start it at their mean and push it by the mean of their motion.</div>`;
    } else {
      this.listEl.innerHTML = project.subjects
        .map((s, i) => {
          const sel = app.selSubject === s.id;
          const pts = project.trackersOf(s.id);
          const name = this.renaming === s.id
            ? `<input type="text" value="${escapeHtml(s.name)}" maxlength="60">`
            : escapeHtml(s.name);
          const rows = sel && pts.length
            ? `<div class="point-list">${pts.map((p) => this.trackerRow(p)).join("")}</div>`
            : "";
          return `<div class="subject ${sel ? "selected" : ""} ${s.hidden ? "is-hidden" : ""}" data-sid="${s.id}"
                    title="Double-click the name to rename">
              <span class="swatch" style="background:${s.color}"></span>
              <span class="name">${name}</span>
              <span class="sub-status" data-status="${s.id}"></span>
              <span class="key-hint">${i < 9 ? i + 1 : ""}</span>
              <button class="icon" data-act="hide" title="${s.hidden ? "Show" : "Hide"} in viewer">${s.hidden ? "◌" : "◉"}</button>
              <button class="icon" data-act="delete" title="Delete subject">✕</button>
            </div>${rows}`;
        })
        .join("");
      const input = this.listEl.querySelector(".name input");
      if (input) this.bindRename(input);
    }
    this.renderPanel();
    this.renderStatus();
    this.renderPatterns();
  }

  patternCount() {
    return listPatterns().length;
  }

  renderPatterns() {
    const all = listPatterns();
    const persistent = isPersistent();
    if (!all.length) {
      this.patternEl.innerHTML = `<div class="section-head">Patterns <span class="muted small">— reusable template looks</span></div>
        <div class="muted small">Shift+drag a pattern, then “Save to library” in the editor. Saved patterns work in any video.</div>`;
      return;
    }
    this.patternEl.innerHTML = `<div class="section-head">Patterns · ${all.length}${persistent ? "" : ` <span class="st-partial" title="Browser storage is unavailable; the library lives only in this session">temporary</span>`}</div>
      <div class="pattern-list">${all.map((p) => `<div class="pattern" data-pat="${p.id}" title="${escapeHtml(p.name)} · ${p.w}×${p.h} px">
        <img src="data:image/png;base64,${p.tmpl}" alt="">
        <span class="pname">${escapeHtml(p.name)}</span>
        <button class="small" data-act="use-pattern" data-pat="${p.id}" title="Add as a look to the selected template tracker, or start a new tracker from it">Use</button>
        <button class="icon" data-act="del-pattern" data-pat="${p.id}" title="Delete this pattern">×</button>
      </div>`).join("")}</div>
      <div class="row">
        <button class="small" data-act="export-patterns" title="Download the library as JSON">Export library</button>
        <button class="small" data-act="import-patterns" title="Merge a library exported from another browser or machine">Import…</button>
      </div>`;
  }

  trackerRow(p) {
    const sel = this.app.isSelected(p.id);
    return `<div class="point-row ${sel ? "selected" : ""}" data-pid="${p.id}">
      <span class="pid">${trackerLabel(p)}</span>
      <span class="range">${p.start} → ${p.end ?? "end"}</span>
      <span class="badge" data-pstatus="${p.id}"></span></div>`;
  }

  renderPanel() {
    const app = this.app;
    const { project } = app;
    if (app.selTrackers.size > 1) {
      this.renderMulti();
      return;
    }
    const p = app.selTracker != null ? project.tracker(app.selTracker) : null;
    if (p) this.renderTracker(p);
    else if (project.subject(app.selSubject)) this.renderSubject(project.subject(app.selSubject));
    else this.panelEl.innerHTML = `<div class="muted">Select a subject or tracker to edit it.</div>`;
  }

  renderMulti() {
    const app = this.app;
    const { project } = app;
    const pts = app.selectedTrackers();
    const chips = pts
      .map((p) => `<span class="chip" data-select="${p.id}" style="border-color:${project.subject(p.subjectId)?.color}">${trackerLabel(p)}</span>`)
      .join("");
    this.panelEl.innerHTML = `
      <h3>${pts.length} trackers selected</h3>
      <div class="chips">${chips}</div>
      <div class="row"><button class="small primary" data-act="track-sel">▶ Track only these <kbd>⇧G</kbd></button></div>
      <div class="row">
        <button class="small" data-act="manual" title="Toggle manual animation from this frame for each selected tracker">Manual toggle <kbd>M</kbd></button>
        <button class="small" data-act="end">End here <kbd>E</kbd></button>
        <button class="small" data-act="unend">Remove ends <kbd>U</kbd></button>
        <button class="small" data-act="delete">Delete</button>
      </div>
      <div class="muted">Ctrl+click trackers to add/remove · drag on the frame to box-select · Esc clears.</div>`;
  }

  renderSubject(s) {
    const { project } = this.app;
    const n = project.trackersOf(s.id).length;
    const keys = s.offset.slice(0, 80)
      .map((k) => `<span class="chip" data-seek="${k.f}" title="offset ${xy(k.dx, k.dy)}">${k.f}</span>`)
      .join("");
    this.panelEl.innerHTML = `
      <h3><span style="color:${s.color}">●</span> ${escapeHtml(s.name)} <span class="muted">· ${n} tracker${n === 1 ? "" : "s"}</span></h3>
      <div class="kv">
        <span>Position</span><span data-live="final"></span>
        <span>Pushed position</span><span data-live="raw"></span>
      </div>
      <div class="section-head">Offset <span class="muted small">— drag the subject marker's ring (auto-keys)</span></div>
      <div class="kv">
        <span>This frame</span><span data-live="offset"></span>
      </div>
      <div class="row">
        <button class="small" data-act="offset-key" title="Pin the current offset with a key on this frame">Key here</button>
        <button class="small" data-act="offset-del" title="Delete the offset key on this frame">Delete key</button>
        <button class="small" data-act="offset-clear" ${s.offset.length ? "" : "disabled"} title="Remove every offset key">Clear offset</button>
      </div>
      ${keys ? `<div class="muted small">Offset keys${s.offset.length > 80 ? ` (first 80 of ${s.offset.length})` : ""} · <kbd>[</kbd> <kbd>]</kbd> jump</div><div class="chips">${keys}</div>` : ""}
      ${this.boundsPanel(s)}
      ${this.policyPanel(s)}
      ${this.finetunePanel(s)}
      <div class="muted" style="margin-top:8px">Click the video to add a point to <b style="color:${s.color}">${escapeHtml(s.name)}</b>.
        The trackers carry the subject: they start it at their mean and then push it by the mean of their motion.</div>`;
  }

  policyPanel(s) {
    const driftEnd = (s.driftPolicy ?? "flag") === "end";
    const motion = s.motionPolicy ?? "off";
    const N = this.app.meta.frameCount;
    const ends = this.app.project.trackersOf(s.id).filter((p) => this.app.project.end(p) < N || p.autoEnd).length;
    const opt = (v, label) => `<option value="${v}" ${motion === v ? "selected" : ""}>${label}</option>`;
    return `<div class="section-head">Automatic ending <span class="muted small">— stop trackers that leave the bounds</span></div>
      <label class="small check"><input type="checkbox" data-act="drift-end" ${driftEnd ? "checked" : ""}>
        End after a sustained escape (keeps the tracker; <kbd>U</kbd> removes an end)</label>
      <label class="small">Confirm for <input data-act="escape-seconds" type="number" min="0.03" max="5" step="0.01" value="${(s.escapeSeconds ?? 0.1).toFixed(2)}"> s of video</label>
      <div class="section-head">Motion consistency <span class="muted small">— outliers vs. the group's motion</span></div>
      <select data-act="motion-policy" title="Fit the translation/rotation/zoom that most trackers agree on; flag or reject the stragglers">
        ${opt("off", "Off")}${opt("flag", "Flag outliers (inspector)")}${opt("exclude", "Exclude outliers from pushing")}${opt("end", "End after sustained disagreement")}
      </select>
      <div class="row"><button class="small" data-act="remove-ends" ${ends ? "" : "disabled"} title="Remove every end in this subject, restoring retained later keys and results">Remove all ends${ends ? ` (${ends})` : ""} <kbd>U</kbd></button></div>`;
  }

  finetunePanel(s) {
    const app = this.app;
    const editing = app.viewer.mode.name === "finetune";
    const rows = s.layers.map((l) => {
      const st = app.project.store(l.keys);
      const ext = st?.extent();
      return `<div class="layer-row" data-layer="${l.id}">
        <input type="radio" name="active-layer" data-act="layer-active" data-layer="${l.id}" ${s.activeLayer === l.id ? "checked" : ""} title="Active layer (R nudges this one)">
        <label class="check" title="Enabled"><input type="checkbox" data-act="layer-enabled" data-layer="${l.id}" ${l.enabled ? "checked" : ""}></label>
        <span class="pname">${escapeHtml(l.name)}</span>
        <input type="range" data-act="layer-weight" data-layer="${l.id}" min="0" max="2" step="0.05" value="${l.weight}" title="Weight (layers stack in order)">
        <span class="muted small">${ext ? `${ext[0]}–${ext[1]}` : "no keys"}</span>
        <button class="icon" data-act="layer-del" data-layer="${l.id}" title="Delete this layer">×</button>
      </div>`;
    }).join("");
    return `<div class="section-head">Finetune <span class="muted small">— additive layers over the pushed position</span></div>
      <div class="row">
        <button class="small primary" data-act="layer-add">+ Layer</button>
        <button class="small ${editing ? "active" : ""}" data-act="finetune-mode" title="Drag anywhere on the frame to nudge the active layer relatively">${editing ? "Done" : "Nudge"} <kbd>R</kbd></button>
        <button class="small" data-act="layer-clear" ${s.layers.length ? "" : "disabled"} title="Remove every key in the active layer">Clear layer</button>
      </div>
      ${rows || `<div class="muted small">No layers yet. Add one, then drag: movement is relative, Shift is 1/10 speed, Q shows a loupe, ← → step frames and hold the value.</div>`}`;
  }

  boundsPanel(s) {
    const { project, viewer } = this.app;
    const ext = project.boundsExtent(s);
    const editing = viewer.mode.name === "bounds";
    const pp = puppeteerSettings();
    const presets = Object.entries(PRESETS).map(([k, v]) => `<option value="${k}" ${k === pp.preset ? "selected" : ""}>${v.label}</option>`).join("");
    const passes = (s.boundsPasses || []).slice().sort((a, b) => (b.level ?? 0) - (a.level ?? 0) || a.id - b.id);
    const levels = [...new Set(passes.map((p) => p.level ?? 0))].sort((a, b) => b - a);
    const takeLevel = levels.length ? levels[0] : 0;
    const passRows = passes.map((p) => this.passRow(p)).join("");
    return `<div class="section-head">Bounds <span class="muted small">— box that guides trackers and flags drift</span></div>
      <div class="kv">
        <span>This frame</span><span data-live="bounds"></span>
        <span>Range</span><span>${ext ? `<span class="chip" data-seek="${ext[0]}">${ext[0]}</span> → <span class="chip" data-seek="${ext[1]}">${ext[1]}</span>` : "none"}</span>
      </div>
      <div class="row">
        <button class="small primary" data-act="puppeteer" title="Follow the subject with the mouse during slowed playback; records a new refinement level">Puppeteer refine <kbd>P</kbd></button>
        <button class="small" data-act="puppeteer-take" ${passes.length ? "" : "disabled"} title="Record another attempt at the same level and average the takes">+ Take L${takeLevel}</button>
        <button class="small ${editing ? "active" : ""}" data-act="bounds-mode" title="Move/resize the manual layer with smooth falloff, or draw one">${editing ? "Done" : "Edit"} <kbd>B</kbd></button>
      </div>
      <div class="row">
        <button class="small" data-act="bounds-clear-here" ${ext ? "" : "disabled"}>Clear from here</button>
        <button class="small" data-act="bounds-clear" ${ext ? "" : "disabled"}>Clear all</button>
      </div>
      <div class="bounds-settings">
        <label title="Capture preset: slower playback with a smaller minimum box can get tighter">Preset <select data-act="pp-preset">${presets}</select></label>
        <label title="Playback speed during a Puppeteer pass">Speed <select data-act="pp-rate">${speeds(pp.rate)}</select></label>
        <label title="How much jiggling the mouse enlarges the box">Size <input data-act="pp-gain" type="range" min="0.25" max="4" step="0.05" value="${pp.gain}"></label>
        <label title="How far (seconds) your hand trails the subject; the pass shifts earlier by this much">Lag <input data-act="pp-lag" type="range" min="0" max="0.8" step="0.01" value="${pp.lag}"></label>
        <label title="Smallest half-size the pass will produce (source pixels)">Min half <input data-act="pp-minhalf" type="number" min="2" max="400" step="1" value="${pp.minHalf}"></label>
        <label title="Padding added to every half-size">Pad <input data-act="pp-pad" type="number" min="0" max="200" step="1" value="${pp.pad}"></label>
        <label title="Edits in Bounds mode fade out over this many seconds each side">Falloff <input data-act="falloff" type="number" min="0.02" max="60" step="0.05" value="${boundsFalloffSeconds().toFixed(2)}"> s</label>
      </div>
      <label class="small check"><input type="checkbox" data-act="bounds-guide" ${s.boundsGuide !== false ? "checked" : ""}>
        Guide CoTracker points with the box's motion</label>
      <div class="section-head">Passes · ${passes.length} <span class="muted small">— averaged within a level; finer levels refine coarser ones</span></div>
      ${passRows || `<div class="muted small">No recorded passes. Puppeteer records one; B edits the manual layer on top (manual wins where set).</div>`}
      <div class="row"><button class="small" data-act="end-drifted" disabled title="End each tracker that is outside the bounds here, from where it left them">End drifted here</button></div>`;
  }

  passRow(p) {
    const tunable = (p.samples || []).length > 0;
    const st = p.settings || {};
    return `<div class="pass-row" data-pass="${p.id}">
      <label class="check" title="Use this pass"><input type="checkbox" data-act="pass-enabled" data-pass="${p.id}" ${p.enabled !== false ? "checked" : ""}></label>
      <span class="pname">${escapeHtml(p.name)}</span>
      <span class="muted small">L${p.level} ${p.kind} · ${p.a}–${p.b}</span>
      <input type="range" data-act="pass-weight" data-pass="${p.id}" min="0" max="1" step="0.05" value="${p.weight}" title="Weight within its level (0 disables it)">
      <button class="icon" data-act="pass-del" data-pass="${p.id}" title="Delete this pass">×</button>
      ${tunable ? `<details class="pass-settings"><summary>tune</summary>
        <label>Lag <input type="number" data-act="pass-lag" data-pass="${p.id}" min="0" max="1" step="0.01" value="${st.lag ?? 0.25}"></label>
        <label>Gain <input type="number" data-act="pass-gain" data-pass="${p.id}" min="0.25" max="4" step="0.05" value="${st.gain ?? 1}"></label>
        <label>Min half <input type="number" data-act="pass-minhalf" data-pass="${p.id}" min="2" max="400" step="1" value="${st.minHalf ?? 16}"></label>
        <label>Pad <input type="number" data-act="pass-pad" data-pass="${p.id}" min="0" max="200" step="1" value="${st.pad ?? 12}"></label>
        <span class="muted small">Regenerates the pass from its recorded mouse samples.</span>
      </details>` : `<span class="muted small" title="Recorded before passes were stored individually; it stays as the base layer">base</span>`}
    </div>`;
  }

  renderTracker(p) {
    const { project } = this.app;
    const s = project.subject(p.subjectId);
    const f = this.frame();
    const name = trackerLabel(p);
    const inManual = !!project.manualRangeAt(p, f);
    const keys = p.keys.slice(0, 80)
      .map((k) => `<span class="chip ${project.manualRangeAt(p, k.f) ? "manual" : ""}" data-seek="${k.f}"
                     title="${k.f === p.start ? "creation point" : project.manualRangeAt(p, k.f) ? "manual keyframe" : "re-anchor"}">${k.f}</span>`)
      .join("");
    const ranges = p.manual
      .map((r) => `<span class="chip manual" data-seek="${r.a}">${r.a} → ${r.b ?? "end"}</span>`)
      .join("");
    const kind = p.kind === "template" ? "template tracker" : "CoTracker point";
    const N = this.app.meta.frameCount;
    const end = project.end(p);
    const auto = p.autoEnd && p.autoEnd.f === end;
    const endText = end >= N ? "end" : String(end);
    const source = auto ? ` <span class="muted small">auto · ${p.autoEnd.reason}</span>`
      : p.end != null ? ` <span class="muted small">user</span>` : "";
    this.panelEl.innerHTML = `
      <h3><span style="color:${s.color}">●</span> ${name} · ${escapeHtml(s.name)} <span class="muted small">${kind}</span></h3>
      <div class="kv">
        <span>Lifespan</span><span>${p.start} → ${endText}${source}
          ${end < N || p.autoEnd ? `<button class="link small" data-act="unend" title="Remove the end and restore the tracker's later keys and results (U)">Remove end</button>` : ""}</span>
        <span>This frame</span><span data-live="state"></span>
        <span>Position</span><span data-live="pos"></span>
        <span>Seed</span><span data-live="seed"></span>
      </div>
      <label class="small check" title="Let sustained escapes end this tracker automatically (reversible; removing an end turns this off)"><input type="checkbox" data-act="auto-end" ${p.noAutoEnd ? "" : "checked"}> Automatic ending</label>
      <div class="row">
        <button class="small" data-act="manual" title="Toggle manual animation from this frame (M)">
          ${inManual ? "Stop manual here" : "Manual from here"} <kbd>M</kbd></button>
        <button class="small" data-act="end" title="End this tracker from this frame on (E, or double-click it)">End here <kbd>E</kbd></button>
        <button class="small" data-act="delete" title="Delete the whole tracker (Del)">Delete</button>
        <button class="small" data-act="track-sel" title="Track only this tracker from the cursor (Shift+G)">▶ Track this <kbd>⇧G</kbd></button>
      </div>
      <div class="muted small">Keyframes${p.keys.length > 80 ? ` (first 80 of ${p.keys.length})` : ""}</div>
      <div class="chips">${keys}</div>
       ${ranges ? `<div class="muted" style="margin-top:6px">Manual ranges</div><div class="chips">${ranges}</div>` : ""}
       ${p.kind === "template" ? this.templatePanel(p) : ""}`;
  }

  templatePanel(p) {
    const images = p.looks.map((l, i) => {
      // Library looks carry their own pixels: never show the current video at
      // the look's coordinates (it may be a different video entirely).
      const src = l.tmpl ? `data:image/png;base64,${l.tmpl}` : api.cropUrl(this.app.meta.id, l.f, l.x, l.y, l.w, l.h);
      return `<div class="template-look" title="Look ${i + 1}, frame ${l.f}${l.tmpl ? " (library pixels)" : ""}">
      <button data-act="edit-look" data-look="${l.id}" title="Edit mask and hotspot"><img alt="Look ${i + 1}" loading="lazy" src="${src}"></button>
      <button class="remove" data-act="remove-look" data-look="${l.id}" ${p.looks.length <= 1 ? "disabled" : ""} title="Remove look">×</button>
    </div>`;
    }).join("");
    return `<div class="section-head">Looks · ${p.looks.length}</div><div class="template-look-list">${images}</div>
      <div class="muted small">Shift+drag another box to add a look to ${trackerLabel(p)}.</div>
      <div class="section-head">Match threshold <span class="threshold-value">${p.threshold.toFixed(2)}</span></div>
      <input data-act="threshold" type="range" min="-1" max="1" step="0.01" value="${p.threshold}">
      <div class="row"><button class="small" data-act="retrack">Re-track from here</button></div>`;
  }

  // Cheap per-frame refresh of status text without rebuilding the DOM.
  renderStatus() {
    const app = this.app;
    const { project, results } = app;
    const f = this.frame();
    for (const el of this.listEl.querySelectorAll("[data-status]")) {
      const s = project.subject(Number(el.dataset.status));
      if (!s) continue;
      const st = project.subjectState(s, f, results);
      let text = "—";
      if (st.status === "unknown") text = `? ${st.pending} to track`;
      else if (st.status === "lost") text = [st.lost && `${st.lost} not found`, st.drifted && `${st.drifted} drifted`].filter(Boolean).join(" · ");
      else if (st.n) {
        text = `${st.visible}/${st.n} visible`;
        if (st.pending) text += ` +${st.pending}?`;
        if (st.lost) text += ` · ${st.lost} lost`;
        if (st.drifted) text += ` · ${st.drifted} drifted`;
      }
      el.textContent = text;
    }
    for (const el of this.listEl.querySelectorAll("[data-pstatus]")) {
      const p = project.tracker(Number(el.dataset.pstatus));
      if (!p) continue;
      const [label, cls] = trackerModeLabel(project.trackerState(p, f, results));
      el.textContent = label;
      el.className = `badge ${cls}`;
    }
    const set = (k, v) => {
      const el = this.panelEl.querySelector(`[data-live="${k}"]`);
      if (el) el.innerHTML = v;
    };
    const p = app.selTracker != null && app.selTrackers.size <= 1 ? project.tracker(app.selTracker) : null;
    if (p) {
      const st = project.trackerState(p, f, results);
      const [label, cls] = trackerModeLabel(st);
      const vis = st?.mode === "auto" ? (p.kind === "template"
        ? ` · score ${st.score?.toFixed(3) ?? "—"} · look ${st.lookIndex >= 0 ? st.lookIndex + 1 : "old"}`
        : ` · vis ${(st.vis * 100).toFixed(0)}%`) : "";
      const subj = project.subject(p.subjectId);
      const outlier = st && (subj?.motionPolicy ?? "off") !== "off" && project.isMotionOutlier(p, f, results);
      set("state", `<span class="badge ${cls}">${label}</span>${vis}${outlier ? ` · <span class="st-partial">motion outlier</span>` : ""}`);
      const off = st && st.x != null && !project.isInside(st.x, st.y) ? " · off-frame" : "";
      set("pos", st && st.x != null ? `${xy(st.x, st.y)}${st.mode === "pending" ? " (last known)" : ""}${off}` : "—");
      set("seed", st?.seg ? `frame ${st.seg.q}` : st?.mode === "manual" ? "manual" : "—");
      const btn = this.panelEl.querySelector('[data-act="manual"]');
      if (btn) btn.firstChild.textContent = project.manualRangeAt(p, f) ? "Stop manual here " : "Manual from here ";
      return;
    }
    const s = !app.selTrackers.size ? project.subject(app.selSubject) : null;
    if (s) {
      const st = project.subjectState(s, f, results);
      set("final", st.x != null ? xy(st.x, st.y) : st.status === "none" ? "no trackers on this frame" : st.status === "lost" ? (st.lost ? "not found on this frame" : "every tracker drifted outside the bounds") : "unknown until tracked");
      set("raw", st.rawX != null ? xy(st.rawX, st.rawY) : "—");
      const off = project.offsetAt(s, f);
      const key = project.offsetKeyAt(s, f);
      const how = key ? "key" : s.offset.length ? "interpolated" : "none";
      set("offset", `${xy(off.dx, off.dy)} <span class="muted">(${how})</span>`);
      const del = this.panelEl.querySelector('[data-act="offset-del"]');
      if (del) del.disabled = !key;
      const pin = this.panelEl.querySelector('[data-act="offset-key"]');
      if (pin) pin.disabled = !!key || !s.offset.length;
      const b = project.boundsAt(s, f);
      set("bounds", b ? `${xy(b[0], b[1])} · ${Math.round(b[2])}×${Math.round(b[3])}` : "none");
      const fall = this.panelEl.querySelector('[data-act="falloff"]');
      if (fall && document.activeElement !== fall) fall.value = boundsFalloffSeconds().toFixed(2);
      const drift = this.panelEl.querySelector('[data-act="end-drifted"]');
      if (drift) {
        const n = st.drifted;
        drift.disabled = !n;
        drift.textContent = n ? `End drifted here (${n})` : "End drifted here";
      }
    }
  }

  onListClick(e) {
    const app = this.app;
    if (e.target.closest("input")) return;
    const rowEl = e.target.closest(".point-row");
    if (rowEl) {
      const p = app.project.tracker(Number(rowEl.dataset.pid));
      if (p && (e.ctrlKey || e.metaKey)) {
        app.select({ tracker: p.id, toggle: true });
      } else if (p) {
        app.select({ subject: p.subjectId, tracker: p.id });
        const f = app.cursor;
        if (f < p.start || f >= app.project.end(p)) app.seek(p.start);
      }
      return;
    }
    const el = e.target.closest(".subject");
    if (!el) return;
    const sid = Number(el.dataset.sid);
    const act = e.target.closest("[data-act]")?.dataset.act;
    const s = app.project.subject(sid);
    if (act === "hide") {
      app.edit(s.hidden ? "Show subject" : "Hide subject", () => app.project.updateSubject(sid, { hidden: !s.hidden }));
    } else if (act === "delete") {
      const n = app.project.trackersOf(sid).length;
      if (!n || confirm(`Delete “${s.name}” and its ${n} tracker(s)? (Ctrl+Z undoes this)`)) {
        app.edit("Delete subject", () => app.project.removeSubject(sid));
      }
    } else if (app.selSubject !== sid || app.selTracker != null) {
      app.select({ subject: sid, tracker: null });
    }
  }

  onPanelClick(e) {
    const app = this.app;
    const seek = e.target.closest("[data-seek]");
    if (seek) {
      app.seek(Number(seek.dataset.seek));
      return;
    }
    const pick = e.target.closest("[data-select]");
    if (pick) {
      app.select({ tracker: Number(pick.dataset.select) });
      return;
    }
    const act = e.target.closest("[data-act]")?.dataset.act;
    const f = app.editFrame();
    const s = app.project.subject(app.selSubject);
    if (act === "track-sel") app.goSelected();
    else if (act === "manual") app.toggleManual();
    else if (act === "end") app.endSelectedHere();
    else if (act === "delete") app.deleteSelectedTrackers();
    else if (act === "unend") app.removeEndSelected();
    else if (act === "remove-ends" && s) app.removeEndsOfSubject(s.id);
    else if (act === "offset-key" && s) {
      const off = app.project.offsetAt(s, f);
      app.setOffsetKey(s.id, f, off.dx, off.dy);
    } else if (act === "offset-del" && s) app.deleteOffsetKey(s.id, f);
    else if (act === "offset-clear" && s) app.clearOffset(s.id);
    else if (act === "edit-look") app.openLookEditor(Number(e.target.closest("[data-look]").dataset.look));
    else if (act === "remove-look") {
      const p = app.selectedTracker();
      if (p?.kind === "template") app.editTemplateLook("Remove template look", [p.id], app.editFrame(),
        () => app.project.removeLook(p.id, Number(e.target.closest("[data-look]").dataset.look)));
    } else if (act === "retrack") app.retrackSelectedHere();
    else if (act === "puppeteer") {
      app.puppeteerTarget = { kind: "refine" };
      app.setMode("puppeteer");
    } else if (act === "puppeteer-take" && s) {
      const levels = (s.boundsPasses || []).map((p) => p.level ?? 0);
      app.puppeteerTarget = { kind: "take", level: levels.length ? Math.max(...levels) : 0 };
      app.setMode("puppeteer");
    } else if (act === "pass-del" && s) app.removeBoundsPass(s.id, Number(e.target.closest("[data-pass]").dataset.pass));
    else if (act === "layer-add" && s) app.addFinetuneLayer(s.id);
    else if (act === "layer-active" && s) app.setActiveLayer(s.id, Number(e.target.closest("[data-layer]").dataset.layer));
    else if (act === "layer-del" && s) app.removeLayer(s.id, Number(e.target.closest("[data-layer]").dataset.layer));
    else if (act === "layer-clear" && s) app.clearLayer(s.id, s.activeLayer ?? s.layers[0]?.id);
    else if (act === "finetune-mode") app.setMode("finetune");
    else if (act === "bounds-mode") app.setMode("bounds");
    else if (act === "bounds-clear" && s) app.clearBounds(s.id, 0);
    else if (act === "bounds-clear-here" && s) app.clearBounds(s.id, f);
    else if (act === "end-drifted" && s) app.endDriftedHere(s.id);
    if (act) e.target.closest("button")?.blur();
  }

  startRename(sid) {
    this.renaming = sid;
    this.render();
  }

  bindRename(input) {
    const sid = this.renaming;
    input.focus();
    input.select();
    let done = false;
    const finish = (commit) => {
      if (done) return;
      done = true;
      this.renaming = null;
      const name = input.value.trim();
      const s = this.app.project.subject(sid);
      if (commit && s && name && name !== s.name) {
        this.app.edit("Rename subject", () => this.app.project.updateSubject(sid, { name }));
      } else this.render();
    };
    input.addEventListener("keydown", (e) => {
      e.stopPropagation();
      if (e.key === "Enter") finish(true);
      else if (e.key === "Escape") finish(false);
    });
    input.addEventListener("blur", () => finish(true));
  }
}
