import { trackerLabel, VIS_THRESHOLD } from "./project.js";
import { trackerModeLabel } from "./sidebar.js";
import { escapeHtml, fmt, timecode, toast } from "./util.js";

const r3 = (v) => (v == null ? null : Math.round(v * 1000) / 1000);

// Debug view of everything known about the current frame.
export class Inspector {
  constructor(app) {
    this.app = app;
    this.el = document.getElementById("inspector");
    this._timer = null;
    document.getElementById("btn-copy-frame").addEventListener("click", () => this.copy());
  }

  frame() {
    return this.app.shown >= 0 ? this.app.shown : this.app.cursor;
  }

  requestRender() {
    if (this._timer) return;
    this._timer = setTimeout(() => {
      this._timer = null;
      this.render();
    }, 80);
  }

  frameData(f) {
    const { project, results, meta } = this.app;
    return {
      frame: f,
      time: f / meta.fps,
      subjects: project.subjects.map((s) => {
        const st = project.subjectState(s, f, results);
        return {
          id: s.id,
          name: s.name,
          status: st.status,
          x: r3(st.x ?? null),
          y: r3(st.y ?? null),
          rawX: r3(st.rawX ?? null),
          rawY: r3(st.rawY ?? null),
          offsetX: r3(st.offX ?? null),
          offsetY: r3(st.offY ?? null),
          finetuneX: r3(st.fineX ?? null),
          finetuneY: r3(st.fineY ?? null),
          trackers: st.n,
          visibleTrackers: st.visible,
          hiddenTrackers: st.hidden,
           pendingTrackers: st.pending,
           notFoundTrackers: st.lost,
          driftedTrackers: st.drifted,
          outliers: st.outliers,
          bounds: project.boundsAt(s, f)?.map(r3) ?? null,
          driftPolicy: s.driftPolicy ?? "flag",
          escapeSeconds: s.escapeSeconds ?? 0.1,
          motionPolicy: s.motionPolicy ?? "off",
          boundsPasses: (s.boundsPasses || []).map((p) => ({ id: p.id, name: p.name, level: p.level, kind: p.kind,
            enabled: p.enabled !== false, weight: p.weight, a: p.a, b: p.b })),
          composite: project.compositeAt(s, f)?.box?.map(r3) ?? null,
          trackerData: project.trackersOf(s.id)
            .map((p) => ({ p, st: project.trackerState(p, f, results) }))
            .filter(({ st }) => st)
            .map(({ p, st }) => ({
              id: p.id,
              label: trackerLabel(p),
              kind: p.kind,
              mode: st.mode,
              x: r3(st.x ?? null),
              y: r3(st.y ?? null),
               visibility: st.mode === "auto" ? r3(st.vis) : st.mode === "manual" ? 1 : null,
               score: r3(st.score ?? null),
               look: st.look ?? null,
               lookIndex: st.lookIndex ?? null,
               notFound: !!st.lost,
              drifted: !!st.drifted,
              outlier: (s.motionPolicy ?? "off") !== "off" ? project.isMotionOutlier(p, f, results) : false,
              contributes: project.contributes(st),
              offFrame: st.x != null ? !project.isInside(st.x, st.y) : null,
              seedFrame: st.seg ? st.seg.q : null,
              end: project.end(p) < meta.frameCount ? project.end(p) : null,
              autoEnd: p.autoEnd ? { ...p.autoEnd } : null,
              noAutoEnd: !!p.noAutoEnd,
            })),
        };
      }),
    };
  }

  render() {
    const app = this.app;
    if (!app.ready) {
      this.el.innerHTML = "";
      return;
    }
    const f = this.frame();
    const data = this.frameData(f);
    const html = [`<div class="frame-line">Frame <b>${f}</b> · ${timecode(f, app.meta.fps)} · visibility threshold ${VIS_THRESHOLD}</div>`];
    if (!data.subjects.length) html.push(`<div class="muted">No subjects yet.</div>`);
    for (const s of data.subjects) {
      const subj = app.project.subject(s.id);
      const off = s.x != null && !app.project.isInside(s.x, s.y) ? ` <span class="st-partial">off-frame</span>` : "";
      const pos = s.x != null
        ? `x ${fmt(s.x)} · y ${fmt(s.y)}${off} &nbsp; <span class="muted">(${fmt(s.x / app.meta.width, 4)}, ${fmt(s.y / app.meta.height, 4)} norm)</span>`
        : s.status === "none" ? "no trackers on this frame" : s.status === "lost" ? (s.notFoundTrackers ? "not found on this frame" : "every tracker drifted outside the bounds") : "position unknown until tracked";
      const counts = s.status === "none" ? "" : ` · ${s.visibleTrackers} visible, ${s.hiddenTrackers} hidden, ${s.pendingTrackers} pending, ${s.notFoundTrackers} not found` +
        `${s.driftedTrackers ? `, ${s.driftedTrackers} drifted` : ""}${s.outliers ? `, ${s.outliers} motion outlier(s)` : ""}`;
      const bounds = s.bounds ? `<div class="subj-pos muted">bounds ${fmt(s.bounds[0])}, ${fmt(s.bounds[1])} · ${fmt(s.bounds[2])}×${fmt(s.bounds[3])}</div>` : "";
      const passes = s.boundsPasses.length
        ? `<div class="subj-pos muted">${s.boundsPasses.length} bounds pass(es)${s.composite ? ` · composite ${fmt(s.composite[0])}, ${fmt(s.composite[1])} · ${fmt(s.composite[2])}×${fmt(s.composite[3])}` : ""}</div>`
        : "";
      const policies = `<div class="subj-pos muted">auto-end ${s.driftPolicy}${s.driftPolicy === "end" ? ` (${s.escapeSeconds}s)` : ""} · motion ${s.motionPolicy}</div>`;
      const stack = s.x != null && (s.offsetX || s.offsetY || s.finetuneX || s.finetuneY)
        ? `<div class="subj-pos muted">pushed ${fmt(s.rawX)}, ${fmt(s.rawY)} + offset ${fmt(s.offsetX)}, ${fmt(s.offsetY)}` +
          `${s.finetuneX || s.finetuneY ? ` + finetune ${fmt(s.finetuneX)}, ${fmt(s.finetuneY)}` : ""}</div>`
        : "";
      const rows = s.trackerData
        .map((p) => {
          let [label] = trackerModeLabel(app.project.trackerState(app.project.tracker(p.id), f, app.results));
          if (p.offFrame) label += " ↗";
          const cls = [
            app.isSelected(p.id) ? "sel" : "",
             p.kind !== "template" && p.mode === "auto" && p.visibility < VIS_THRESHOLD ? "is-hidden" : "",
            !p.contributes ? "is-pending" : "",
          ].join(" ");
          return `<tr class="${cls}"><td>${p.label}</td><td>${label}</td><td>${fmt(p.x)}</td><td>${fmt(p.y)}</td>
             <td>${p.kind === "template" ? p.score == null ? "—" : p.score.toFixed(3) : p.visibility == null ? "—" : (p.visibility * 100).toFixed(0) + "%"}</td>
             <td>${p.lookIndex == null ? "—" : p.lookIndex < 0 ? "old" : p.lookIndex + 1}</td><td>${p.seedFrame ?? "—"}${p.outlier ? " ⚠" : ""}</td></tr>`;
        })
        .join("");
      const ends = s.trackerData
        .filter((p) => p.end != null || p.autoEnd)
        .map((p) => `<span class="chip" title="Remove an end with U">${p.label} ${p.autoEnd ? `auto · ${p.autoEnd.reason}` : "user"} @${p.end ?? p.autoEnd.f}${p.noAutoEnd ? " (auto off)" : ""}</span>`)
        .join(" ");
      html.push(`<div class="subj ${s.id === app.selSubject ? "sel" : ""}">
        <div class="subj-head"><span class="swatch" style="background:${subj.color}"></span>${escapeHtml(s.name)}
          <span class="st-${s.status}">${s.status}</span></div>
        <div class="subj-pos">${pos}${counts}</div>
        ${stack}${bounds}${passes}${policies}
         ${rows ? `<table><tr><th>trk</th><th>state</th><th>x</th><th>y</th><th>vis/score</th><th>look</th><th>seed</th></tr>${rows}</table>` : ""}
        ${ends ? `<div class="chips">${ends}</div>` : ""}
      </div>`);
    }
    this.el.innerHTML = html.join("");
  }

  async copy() {
    if (!this.app.ready) return;
    const text = JSON.stringify(this.frameData(this.frame()), null, 2);
    try {
      await navigator.clipboard.writeText(text);
      toast("Frame data copied to clipboard");
    } catch {
      toast("Clipboard unavailable", "error");
    }
  }
}
