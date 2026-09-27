// Motion consistency for a subject's trackers: fit the 2D similarity transform
// (translation + rotation + uniform zoom) that most dots agree on, then measure
// each dot's residual against it. This catches a straggler even when every dot
// has a different motion vector — dots expanding outward, contracting inward or
// circling a point all fit the same model.
//
// Pure and deterministic (no DOM, no project access) so it can be unit-tested:
// see editor/tests/test_motion.mjs and editor/PLAN.md §8.
//
// Model:  x' = a*x - b*y + tx,  y' = b*x + a*y + ty   (a = s·cosθ, b = s·sinθ)
// Inputs are arrays of {x, y, ok} — `ok: false` marks a sample that cannot be
// used as evidence (hidden, not found, outside the frame, …).

export const MIN_POINTS = 4; // never declare a consensus with fewer dots than this
export const MIN_SEPARATION = 6; // px between a pair used to propose a model
export const DEFAULT_FLOOR = 1.5; // px absolute residual floor (tracking noise)
export const CONSENSUS_FRACTION = 2 / 3; // agreeing share required to trust a fit

const finite = (v) => Number.isFinite(v);

export function usablePoints(pts) {
  const out = [];
  for (let i = 0; i < (pts?.length || 0); i++) {
    const p = pts[i];
    if (p && p.ok !== false && finite(p.x) && finite(p.y)) out.push(i);
  }
  return out;
}

// Exact similarity taking prev[i] -> cur[i] for a single pair.
export function similarityFromPair(a0, a1, b0, b1) {
  const zr = a1.x - a0.x;
  const zi = a1.y - a0.y;
  const wr = b1.x - b0.x;
  const wi = b1.y - b0.y;
  const d = zr * zr + zi * zi;
  if (d < 1e-9) return null;
  const a = (wr * zr + wi * zi) / d;
  const b = (wi * zr - wr * zi) / d;
  return finish(a, b, a0, b0);
}

function finish(a, b, a0, b0) {
  return { a, b, tx: b0.x - (a * a0.x - b * a0.y), ty: b0.y - (b * a0.x + a * a0.y) };
}

// Least-squares similarity over all pairs [[x0,y0,x1,y1], …].
export function fitSimilarity(pairs) {
  if (!pairs.length) return null;
  let mx = 0;
  let my = 0;
  let mu = 0;
  let mv = 0;
  for (const [x, y, u, v] of pairs) {
    mx += x;
    my += y;
    mu += u;
    mv += v;
  }
  const n = pairs.length;
  mx /= n;
  my /= n;
  mu /= n;
  mv /= n;
  let sxx = 0;
  let num = 0;
  let den = 0;
  for (const [x, y, u, v] of pairs) {
    const xc = x - mx;
    const yc = y - my;
    const uc = u - mu;
    const vc = v - mv;
    sxx += xc * xc + yc * yc;
    num += xc * uc + yc * vc;
    den += xc * vc - yc * uc;
  }
  if (sxx < 1e-9) return null;
  const a = num / sxx;
  const b = den / sxx;
  const tx = mu - (a * mx - b * my);
  const ty = mv - (b * mx + a * my);
  return { a, b, tx, ty };
}

export function applyModel(m, x, y) {
  return [m.a * x - m.b * y + m.tx, m.b * x + m.a * y + m.ty];
}

export function modelScale(m) {
  return Math.hypot(m.a, m.b);
}

export function modelRotation(m) {
  return Math.atan2(m.b, m.a);
}

function residualsOf(model, prev, cur, ids) {
  return ids.map((i) => {
    const [x, y] = applyModel(model, prev[i].x, prev[i].y);
    return Math.hypot(cur[i].x - x, cur[i].y - y);
  });
}

function median(values) {
  if (!values.length) return 0;
  const s = [...values].sort((p, q) => p - q);
  const h = s.length >> 1;
  return s.length % 2 ? s[h] : (s[h - 1] + s[h]) / 2;
}

// Fit the strongest consensus between prev and cur. Returns
//   { ok, reason?, model, scale, rotation, threshold, inliers: [index],
//     outliers: [index], residuals: Map(index -> px), used: [index] }
// `ok: false` means "insufficient evidence" (small cluster, too close together,
// or a split with no convincing majority) — not that the dots are bad.
export function analyzeMotion(prev, cur, opts = {}) {
  const floor = Math.max(0, Number(opts.floor ?? DEFAULT_FLOOR));
  const minPoints = opts.minPoints ?? MIN_POINTS;
  const minSep = opts.minSeparation ?? MIN_SEPARATION;
  const ids = usablePoints(prev).filter((i) => cur?.[i] && cur[i].ok !== false && finite(cur[i].x) && finite(cur[i].y));
  const base = { ok: false, used: ids, inliers: [], outliers: [], residuals: new Map(), threshold: floor };
  if (ids.length < minPoints) return { ...base, reason: "too few usable dots" };

  // Candidate models from well-separated pairs.
  const pairs = [];
  for (let ii = 0; ii < ids.length; ii++) {
    for (let jj = ii + 1; jj < ids.length; jj++) {
      const i = ids[ii];
      const j = ids[jj];
      const d = Math.hypot(prev[i].x - prev[j].x, prev[i].y - prev[j].y);
      if (d < minSep) continue;
      const m = similarityFromPair(prev[i], prev[j], cur[i], cur[j]);
      if (!m || !finite(m.a) || !finite(m.b)) continue;
      const s = modelScale(m);
      if (s < 0.1 || s > 10) continue;
      pairs.push(m);
    }
  }
  if (!pairs.length) return { ...base, reason: "dots too close together" };

  // Consensus at the noise floor, then refit on the agreeing dots.
  const tol = Math.max(floor, Number(opts.probe ?? floor * 2));
  let best = null;
  for (const m of pairs) {
    const rs = residualsOf(m, prev, cur, ids);
    let count = 0;
    let sum = 0;
    for (const r of rs) if (r <= tol) { count++; sum += r; }
    if (!best || count > best.count || (count === best.count && sum < best.sum)) best = { m, count, sum };
  }
  const inlierIds = ids.filter((i) => {
    const [x, y] = applyModel(best.m, prev[i].x, prev[i].y);
    return Math.hypot(cur[i].x - x, cur[i].y - y) <= tol;
  });
  let model = inlierIds.length >= 2 ? fitSimilarity(inlierIds.map((i) => [prev[i].x, prev[i].y, cur[i].x, cur[i].y])) : null;
  if (!model || !finite(model.a) || !finite(model.b)) model = best.m;

  // Robust threshold: the absolute floor, or three robust sigmas of the
  // *consensus* residuals when the tracking noise is larger than the floor.
  // Using the inliers (not all dots) keeps a 50/50 split from inflating it.
  const rs = residualsOf(model, prev, cur, ids);
  const sigma = 1.4826 * median(residualsOf(model, prev, cur, inlierIds));
  const threshold = Math.max(floor, 3 * sigma);
  const inliers = [];
  const outliers = [];
  const residualMap = new Map();
  for (let k = 0; k < ids.length; k++) {
    residualMap.set(ids[k], rs[k]);
    if (rs[k] <= threshold) inliers.push(ids[k]);
    else outliers.push(ids[k]);
  }
  const need = Math.max(minPoints, Math.ceil(ids.length * (opts.consensus ?? CONSENSUS_FRACTION)));
  const scale = modelScale(model);
  const info = {
    used: ids,
    model,
    scale,
    rotation: modelRotation(model),
    threshold,
    inliers,
    outliers,
    residuals: residualMap,
  };
  if (!(scale >= 0.1 && scale <= 10)) return { ...base, ...info, ok: false, reason: "implausible motion model" };
  if (inliers.length < need) return { ...base, ...info, ok: false, reason: "no clear consensus" };
  return { ok: true, ...info };
}

// Combine an adjacent-frame analysis with a longer-interval one. A dot flagged
// by either is an outlier: the adjacent interval catches sudden departures, the
// long interval catches gradual drift that a single frame cannot see. Runs of
// consecutive flags (quality.js) keep one-frame glitches from becoming ends.
export function combineAnalyses(a, b) {
  const has = (x) => x && x.ok;
  if (!has(a) && !has(b)) return a || b;
  if (!has(a)) return b;
  if (!has(b)) return a;
  const outliers = [...new Set([...a.outliers, ...b.outliers])];
  const inliers = a.inliers.filter((i) => !outliers.includes(i));
  return { ...a, outliers, inliers, combined: true, both: a.outliers.filter((i) => b.outliers.includes(i)) };
}
