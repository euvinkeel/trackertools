// Deterministic tracker quality: sustained escapes from the composite bounds
// (and, optionally, persistent motion-consistency outliers) become reversible
// automatic ends. Analysis always runs on raw observations — it ignores the
// automatic ends it produces — so it cannot hide the evidence it is based on.
//
// Pure logic on top of Project/ResultStore; no DOM. See editor/PLAN.md §5.

import { analyzeMotion, combineAnalyses } from "./motion.js";

export const ESCAPE_DEFAULT_S = 0.1; // sustained escape before an automatic end
export const MOTION_LOOKBACK_S = 0.25; // long interval for motion comparison
export const MOTION_END_S = 0.2; // sustained motion disagreement before an end

export function confirmationFrames(fps, seconds) {
  return Math.max(3, Math.round(fps * seconds));
}

export function motionFloor(width) {
  // CoTracker runs at 512 px wide; a model pixel of tracking noise is
  // width/512 source pixels. Keep a small absolute floor for tiny videos.
  return Math.max(1.5, (Number(width) || 1920) / 512 * 1.5);
}

// Motion-consistency outliers for one subject on one frame: a Set of tracker
// ids whose observed motion disagrees with the cohort's similarity transform on
// both the adjacent frame and a ~0.25 s interval. Cached per project version.
export function motionOutlierSet(project, results, s, f, opts = {}) {
  const cache = project.cache;
  if (!cache.motion) cache.motion = new Map();
  const key = `${s.id}:${f}`;
  if (cache.motion.has(key)) return cache.motion.get(key);
  const fps = Number(opts.fps) || 30;
  const floor = opts.floor ?? motionFloor(project.width);
  const look = Math.max(1, Math.round(fps * MOTION_LOOKBACK_S));
  const trackers = project.trackersOf(s.id);
  const out = new Set();
  // Candidates for rejection: anything tracked on this frame, even drifted.
  const candidateIds = new Set(trackers
    .filter((p) => {
      const st = project.trackerState(p, f, results);
      return st && st.mode === "auto" && !st.lost && st.x != null && project.isInside(st.x, st.y);
    })
    .map((p) => p.id));
  if (candidateIds.size >= 4) {
    const adjacent = f - 1 >= 0 ? combinePair(project, results, trackers, f - 1, f, floor, opts) : null;
    const long = f - look >= 0 ? combinePair(project, results, trackers, f - look, f, floor, opts) : null;
    const both = adjacent && long ? combineAnalyses(adjacent, long) : adjacent || long;
    if (both?.ok) for (const i of both.outliers) if (candidateIds.has(i)) out.add(i);
  }
  cache.motion.set(key, out);
  if (cache.motion.size > 4096) cache.motion.clear();
  return out;
}

function combinePair(project, results, trackers, g, f, floor, opts) {
  const prev = [];
  const cur = [];
  for (const p of trackers) {
    const a = project.trackerState(p, g, results);
    const b = project.trackerState(p, f, results);
    const okA = !!a && a.mode === "auto" && !a.lost && a.x != null && project.isInside(a.x, a.y) && !a.drifted && a.vis >= 0.6;
    const okB = !!b && b.mode === "auto" && !b.lost && b.x != null && project.isInside(b.x, b.y);
    prev.push({ id: p.id, x: a?.x ?? NaN, y: a?.y ?? NaN, ok: okA });
    cur.push({ id: p.id, x: b?.x ?? NaN, y: b?.y ?? NaN, ok: okB });
  }
  const res = analyzeMotion(prev, cur, { floor, ...opts });
  // Map array indices (already tracker order) to ids.
  const map = (arr) => arr.map((i) => trackers[i].id);
  const residuals = new Map();
  for (const [i, r] of res.residuals) residuals.set(trackers[i].id, r);
  return { ...res, inliers: map(res.inliers), outliers: map(res.outliers), residuals };
}

// Automatic-end candidates for one subject from stored observations. Returns a
// Map trackerId -> { f, confirmedAt, reason } with the *first frame* of each
// confirmed escape/outlier run (not the frame that confirmed it).
export function scanAutoEnds(project, results, s, opts = {}) {
  const fps = Number(opts.fps) || 30;
  const boundsFrames = confirmationFrames(fps, opts.escapeSeconds ?? ESCAPE_DEFAULT_S);
  const motionFrames = confirmationFrames(fps, opts.motionSeconds ?? MOTION_END_S);
  const boundsPolicy = (s.driftPolicy ?? "flag") === "end";
  const motionPolicy = (s.motionPolicy ?? "off") === "end";
  const out = new Map();
  if (!boundsPolicy && !motionPolicy) return out;
  for (const p of project.trackersOf(s.id)) {
    if (p.noAutoEnd) continue;
    const end = project.rawEnd(p);
    let runStart = -1;
    let runLen = 0;
    let reason = null;
    const test = (f) => {
      const st = project.trackerState(p, f, results, end);
      if (!st || st.mode !== "auto" || st.lost) return null;
      if (boundsPolicy && st.drifted) return "bounds";
      if (motionPolicy && motionOutlierSet(project, results, s, f, opts).has(p.id)) return "motion";
      return null;
    };
    for (let f = p.start; f < end; f++) {
      const why = test(f);
      if (why) {
        if (runLen === 0) runStart = f;
        runLen++;
        reason = why;
        const need = why === "motion" ? motionFrames : boundsFrames;
        if (runLen >= need) break;
      } else {
        runStart = -1;
        runLen = 0;
      }
    }
    if (runLen > 0) {
      const need = reason === "motion" ? motionFrames : boundsFrames;
      if (runLen >= need) out.set(p.id, { f: runStart, confirmedAt: runStart + runLen - 1, reason });
    }
  }
  return out;
}

// Recompute a subject's automatic ends from stored observations. Mutates the
// project (no undo entry: this is derived state) and returns the trackers whose
// end changed, so a running job can retire them. `touch` is called by the
// caller.
export function applyAutoEnds(project, results, s, opts = {}) {
  const candidates = scanAutoEnds(project, results, s, opts);
  const changed = [];
  for (const p of project.trackersOf(s.id)) {
    if (p.noAutoEnd) {
      if (p.autoEnd) {
        delete p.autoEnd;
        changed.push(p);
      }
      continue;
    }
    const cand = candidates.get(p.id);
    if (cand) {
      if (!p.autoEnd || p.autoEnd.f !== cand.f || p.autoEnd.reason !== cand.reason) {
        p.autoEnd = { f: cand.f, confirmedAt: cand.confirmedAt, reason: cand.reason, boundsRev: project.boundsRev };
        changed.push(p);
      }
    } else if (p.autoEnd) {
      delete p.autoEnd;
      changed.push(p);
    }
  }
  return changed;
}
