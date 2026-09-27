import { trackerLabel, VIS_THRESHOLD } from "./project.js";
import { download, toast } from "./util.js";

const r3 = (v) => (v == null ? null : Math.round(v * 1000) / 1000);

// Open, documented interchange format (see editor/README.md). Columnar arrays
// keep long videos compact and trivial to load in any language. `x`/`y` are
// always the final subject position (v1 readers keep working); v2 adds the
// parts it is made of: the position the trackers pushed it to + offset + finetune.
export function buildExport(app) {
  const { project, results, meta } = app;
  const N = meta.frameCount;
  const subjects = project.subjects.map((s) => {
    const track = {
      frame: [], x: [], y: [], rawX: [], rawY: [], offsetX: [], offsetY: [], finetuneX: [], finetuneY: [],
       status: [], trackers: [], visibleTrackers: [], hiddenTrackers: [], pendingTrackers: [], notFoundTrackers: [],
      driftedTrackers: [], outliers: [], bounds: [],
    };
    for (let f = 0; f < N; f++) {
      const st = project.subjectState(s, f, results);
      if (st.status === "none") continue;
      track.driftedTrackers.push(st.drifted);
      track.outliers.push(st.outliers ?? 0);
      track.bounds.push(project.boundsAt(s, f)?.map(r3) ?? null);
      track.frame.push(f);
      track.x.push(r3(st.x));
      track.y.push(r3(st.y));
      track.rawX.push(r3(st.rawX));
      track.rawY.push(r3(st.rawY));
      track.offsetX.push(r3(st.offX));
      track.offsetY.push(r3(st.offY));
      track.finetuneX.push(r3(st.fineX));
      track.finetuneY.push(r3(st.fineY));
      track.status.push(st.status);
      track.trackers.push(st.n);
      track.visibleTrackers.push(st.visible);
      track.hiddenTrackers.push(st.hidden);
       track.pendingTrackers.push(st.pending);
       track.notFoundTrackers.push(st.lost);
    }
    const trackers = project.trackersOf(s.id).map((p) => {
      const samples = { frame: [], x: [], y: [], visibility: [], score: [], lookSlot: [], look: [], notFound: [], drifted: [], mode: [], contributes: [], active: [] };
      // Raw lifespan: dormant frames past an end are retained data and must be
      // exported too (their mode shows they are outside the effective end).
      for (let f = p.start; f < project.rawEnd(p); f++) {
        const st = project.trackerState(p, f, results, project.rawEnd(p));
        if (!st) continue;
        samples.frame.push(f);
        samples.x.push(r3(st.x));
        samples.y.push(r3(st.y));
         samples.visibility.push(st.mode === "auto" ? r3(st.vis) : st.mode === "manual" ? 1 : null);
         samples.score.push(r3(st.score));
         samples.lookSlot.push(st.look ?? null);
         samples.look.push(st.lookIndex >= 0 ? st.lookIndex : null);
         samples.notFound.push(!!st.lost);
        samples.drifted.push(!!st.drifted);
        samples.mode.push(st.mode);
        samples.contributes.push(project.contributes(st));
        samples.active.push(f < project.end(p));
      }
      return {
        id: p.id,
        label: trackerLabel(p),
         kind: p.kind,
         ...(p.kind === "template" ? { threshold: p.threshold, looks: p.looks.map((l) => ({ ...l, tmpl: l.tmpl ? "<embedded PNG>" : null })),
           retiredLooks: (p.retiredLooks || []).map((l) => ({ ...l, tmpl: l.tmpl ? "<embedded PNG>" : null })) } : {}),
        start: p.start,
        end: p.end,
        autoEnd: p.autoEnd ? { ...p.autoEnd } : null,
        noAutoEnd: !!p.noAutoEnd,
        effectiveEnd: project.end(p) < N ? project.end(p) : null,
        keyframes: p.keys.map((k) => ({ frame: k.f, x: r3(k.x), y: r3(k.y), kind: k.f === p.start ? "create" : project.manualRangeAt(p, k.f) ? "manual" : "anchor" })),
        manualRanges: p.manual.map((r) => ({ start: r.a, end: r.b ?? project.end(p) })),
        samples,
      };
    });
    return {
      id: s.id,
      name: s.name,
      color: s.color,
      offsetKeys: s.offset.map((k) => ({ frame: k.f, dx: r3(k.dx), dy: r3(k.dy) })),
      boundsGuide: s.boundsGuide !== false,
      driftPolicy: s.driftPolicy ?? "flag",
      escapeSeconds: s.escapeSeconds ?? 0.1,
      motionPolicy: s.motionPolicy ?? "off",
      boundsManual: (() => {
        const st = project.boundsStore(s);
        return st && !st.empty ? { frames: st.extent() } : null;
      })(),
      boundsPasses: (s.boundsPasses || []).map((p) => ({
        id: p.id, name: p.name, level: p.level, kind: p.kind, enabled: p.enabled !== false,
        weight: p.weight, range: p.a != null ? [p.a, p.b] : null, settings: p.settings, samples: p.samples.length,
      })),
      track,
      trackers,
    };
  });
  return {
    format: "cotrack.tracks",
    version: 3,
    generator: "CoTrack editor (CoTracker3)",
    exportedAt: new Date().toISOString(),
    video: { name: meta.name, width: meta.width, height: meta.height, fps: meta.fps, frameCount: N, durationSeconds: meta.duration },
    coordinates: {
      units: "source pixels",
      origin: "top-left",
      note: "(0,0) is the top-left corner of the top-left pixel; pixel centers are at +0.5. Divide by width/height for 0..1. Positions may lie outside the frame.",
    },
    visibilityThreshold: VIS_THRESHOLD,
    subjects,
  };
}

function baseName(app) {
  return app.meta.name.replace(/\.[^.]+$/, "");
}

// Normalized (0..1) subject path, 1-based frame numbers: the shape most NLE
// scripts and expressions expect.
export function buildNormalized(app) {
  const { project, results, meta } = app;
  const rows = [];
  for (const s of project.subjects) {
    for (let f = 0; f < meta.frameCount; f++) {
      const st = project.subjectState(s, f, results);
      if (st.x == null) continue;
      rows.push({ subject: s.name, frame: f, time: f / meta.fps, x: st.x / meta.width, y: st.y / meta.height });
    }
  }
  return rows;
}

export function exportNormalizedCSV(app) {
  if (!app.ready) return;
  const lines = ["subject,frame,time_s,x_norm,y_norm"];
  const esc = (s) => (/[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s);
  for (const r of buildNormalized(app)) {
    lines.push([esc(r.subject), r.frame, r.time.toFixed(5), r.x.toFixed(6), r.y.toFixed(6)].join(","));
  }
  download(`${baseName(app)}.normalized.csv`, lines.join("\n"), "text/csv");
  toast(`Exported ${lines.length - 1} normalized positions`);
}

// DaVinci Resolve Fusion Tracker node as a .setting (Lua table). Experimental:
// the file is generated to Fusion's .setting schema but has not been validated
// inside Resolve; see editor/README.md. Coordinates are Fusion-normalized (0..1,
// origin bottom-left).
export function buildFusionSetting(app) {
  const { project, results, meta } = app;
  const tools = [];
  project.subjects.forEach((s, i) => {
    const keys = [];
    for (let f = 0; f < meta.frameCount; f++) {
      const st = project.subjectState(s, f, results);
      if (st.x == null) continue;
      const x = st.x / meta.width;
      const y = 1 - st.y / meta.height; // Fusion's origin is bottom-left
      keys.push(`\t\t\t\t[${f}] = { Pattern = { ${x.toFixed(6)}, ${y.toFixed(6)} }, },`);
    }
    if (!keys.length) return;
    const name = `CoTrack_${s.name.replace(/[^A-Za-z0-9_]/g, "_")}_${i + 1}`;
    tools.push(`\t\t${name} = Tracker {
\t\t\tCtrlWZoom = false,
\t\t\tInputs = {
\t\t\t\tOperation = Input { Value = "Track", },
\t\t\t\tPattern = Input { Value = { ${(keys.length ? 0.5 : 0.5)}, 0.5 }, },
\t\t\t},
\t\t\tKeyFrames = {
${keys.join("\n")}
\t\t\t},
\t\t},`);
  });
  return `{
\tTools = ordered() {
${tools.join("\n")}
\t},
}
`;
}

export function exportFusion(app) {
  if (!app.ready) return;
  const data = buildFusionSetting(app);
  download(`${baseName(app)}.fusion.setting`, data, "text/plain");
  toast("Exported a Fusion .setting — experimental, validate it in Fusion before use.");
}

export function exportJSON(app) {
  if (!app.ready) return;
  const data = buildExport(app);
  download(`${baseName(app)}.tracks.json`, JSON.stringify(data), "application/json");
  toast(`Exported ${data.subjects.length} subject(s) as JSON`);
}

export function exportCSV(app) {
  if (!app.ready) return;
  const { project, results, meta } = app;
  const lines = ["frame,time_s,subject_id,subject,status,x,y,raw_x,raw_y,offset_x,offset_y,finetune_x,finetune_y," +
     "bounds_cx,bounds_cy,bounds_w,bounds_h,trackers,visible_trackers,hidden_trackers,pending_trackers,not_found,drifted,outliers"];
  const esc = (s) => (/[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s);
  const n3 = (v) => (v == null ? "" : v.toFixed(3));
  for (let f = 0; f < meta.frameCount; f++) {
    for (const s of project.subjects) {
      const st = project.subjectState(s, f, results);
      if (st.status === "none") continue;
      const b = project.boundsAt(s, f) || [];
      lines.push([f, (f / meta.fps).toFixed(4), s.id, esc(s.name), st.status, n3(st.x), n3(st.y), n3(st.rawX), n3(st.rawY),
         n3(st.offX), n3(st.offY), n3(st.fineX), n3(st.fineY), n3(b[0]), n3(b[1]), n3(b[2]), n3(b[3]),
         st.n, st.visible, st.hidden, st.pending, st.lost, st.drifted, st.outliers ?? 0].join(","));
    }
  }
  download(`${baseName(app)}.subjects.csv`, lines.join("\n"), "text/csv");
  toast(`Exported ${lines.length - 1} rows as CSV`);
}
