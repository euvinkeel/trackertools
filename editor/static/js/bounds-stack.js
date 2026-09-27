// Composite subject bounds built from recorded Puppeteer passes plus manual
// corrections. A subject owns a list of passes (see project.js); each pass has a
// refinement level. Within a level, takes are averaged (centers robustly, sizes
// in log space) and disagreement adds a modest margin. Across levels, a finer
// level blends over the coarser result inside its own coverage, with a short
// endpoint cross-fade; outside its coverage the coarser result remains. The
// manual layer is applied by the caller on top (manual wins where defined).
//
// Pure (no DOM, no project access) so it can be unit-tested: see
// editor/tests/test_bounds_stack.mjs and editor/PLAN.md §6.

export const FADE_SECONDS = 0.2;
export const MIN_SIZE = 8;
export const UNCERTAIN_CENTER_FRACTION = 0.15; // of the box size
export const UNCERTAIN_LOG_SIZE = 0.35;

const log = Math.log;
const exp = Math.exp;

function weightedMean(values, weights) {
  let s = 0;
  let w = 0;
  for (let i = 0; i < values.length; i++) {
    s += values[i] * weights[i];
    w += weights[i];
  }
  return w > 0 ? s / w : 0;
}

// Weighted median of a numeric list (robust against one wild take).
function weightedMedian(values, weights) {
  const order = values.map((_, i) => i).sort((a, b) => values[a] - values[b]);
  let total = 0;
  for (const w of weights) total += w;
  let acc = 0;
  for (const i of order) {
    acc += weights[i];
    if (acc >= total / 2) return values[i];
  }
  return values[order[order.length - 1]];
}

function clampSize(box) {
  box[2] = Math.max(MIN_SIZE, box[2]);
  box[3] = Math.max(MIN_SIZE, box[3]);
  return box;
}

// items: [{ box: [cx,cy,w,h], weight }] (already filtered to this frame/level).
// Returns { box, centerSpread, sizeSpread, uncertain, n } or null.
export function combineTakes(items) {
  const live = items.filter((it) => it.box && Number.isFinite(it.box[0]) && it.box[2] > 0 && it.box[3] > 0);
  if (!live.length) return null;
  const weights = live.map((it) => Math.max(1e-6, Number(it.weight ?? 1)));
  const robust = live.length >= 3;
  const cx = robust ? weightedMedian(live.map((it) => it.box[0]), weights) : weightedMean(live.map((it) => it.box[0]), weights);
  const cy = robust ? weightedMedian(live.map((it) => it.box[1]), weights) : weightedMean(live.map((it) => it.box[1]), weights);
  const lw = weightedMean(live.map((it) => log(Math.max(1, it.box[2]))), weights);
  const lh = weightedMean(live.map((it) => log(Math.max(1, it.box[3]))), weights);
  let w = exp(lw);
  let h = exp(lh);
  let c2 = 0;
  let s2 = 0;
  let sw = 0;
  for (let i = 0; i < live.length; i++) {
    const b = live[i].box;
    c2 += weights[i] * ((b[0] - cx) ** 2 + (b[1] - cy) ** 2);
    s2 += weights[i] * ((log(Math.max(1, b[2])) - lw) ** 2 + (log(Math.max(1, b[3])) - lh) ** 2);
    sw += weights[i];
  }
  const centerSpread = Math.sqrt(c2 / Math.max(sw, 1e-9)) / Math.SQRT2;
  const sizeSpread = Math.sqrt(s2 / Math.max(sw, 1e-9));
  // Disagreement widens the intended region a little; it is a margin, not a
  // confidence interval (repeating an identical pass must not shrink the box).
  w = w * exp(sizeSpread * 0.5) + 2 * centerSpread;
  h = h * exp(sizeSpread * 0.5) + 2 * centerSpread;
  const uncertain = centerSpread > Math.max(4, UNCERTAIN_CENTER_FRACTION * w) || sizeSpread > UNCERTAIN_LOG_SIZE;
  return { box: clampSize([cx, cy, w, h]), centerSpread, sizeSpread, uncertain, n: live.length };
}

export function blendBox(a, b, t) {
  const k = Math.max(0, Math.min(1, t));
  return clampSize([
    a[0] + (b[0] - a[0]) * k,
    a[1] + (b[1] - a[1]) * k,
    Math.max(1, a[2] + (b[2] - a[2]) * k),
    Math.max(1, a[3] + (b[3] - a[3]) * k),
  ]);
}

// [a, b] frames the given passes cover (undefined values ignored).
export function passExtent(passes) {
  let a = Infinity;
  let b = -Infinity;
  for (const p of passes) {
    if (p.a == null || p.b == null) continue;
    a = Math.min(a, p.a);
    b = Math.max(b, p.b);
  }
  return a <= b ? [a, b] : null;
}

function levelFade(coverage, f, fadeFrames) {
  if (!coverage || fadeFrames <= 0) return 1;
  const [a, b] = coverage;
  return Math.min(1, (f - a + 1) / fadeFrames, (b - f + 1) / fadeFrames);
}

// passes: records with { enabled, weight, level, a, b }. getBox(pass, f) returns
// the pass's [cx,cy,w,h] on frame f or null. Returns
//   { box, uncertain, centerSpread, sizeSpread, takes, level } or null.
export function compositeBounds(passes, f, getBox, fps = 30) {
  const byLevel = new Map();
  for (const p of passes) {
    if (p.enabled === false || !(Number(p.weight ?? 1) > 0)) continue;
    if (p.a != null && (f < p.a || f > p.b)) continue;
    const box = getBox(p, f);
    if (!box) continue;
    const level = p.level ?? 0;
    if (!byLevel.has(level)) byLevel.set(level, []);
    byLevel.get(level).push({ box, weight: Number(p.weight ?? 1) });
  }
  if (!byLevel.size) return null;
  const fadeFrames = Math.max(1, Math.round(FADE_SECONDS * fps));
  const levels = [...byLevel.keys()].sort((a, b) => a - b);
  let out = null;
  let info = null;
  let finest = null;
  for (const level of levels) {
    const items = byLevel.get(level);
    const c = combineTakes(items);
    if (!c) continue;
    const coverage = levelCoverage(passes, level);
    const strength = Math.min(1, Math.max(...items.map((it) => Number(it.weight ?? 1))));
    const t = strength * levelFade(coverage, f, fadeFrames);
    out = out ? blendBox(out, c.box, t) : c.box;
    info = c;
    finest = level;
  }
  if (!out) return null;
  return { box: out, uncertain: !!info?.uncertain, centerSpread: info?.centerSpread ?? 0,
    sizeSpread: info?.sizeSpread ?? 0, takes: info?.n ?? 1, level: finest };
}

// Union coverage of the passes at one level, [a, b] or null.
export function levelCoverage(passes, level) {
  let a = Infinity;
  let b = -Infinity;
  for (const p of passes) {
    if ((p.level ?? 0) !== level || p.enabled === false) continue;
    if (p.a == null || p.b == null) continue;
    a = Math.min(a, p.a);
    b = Math.max(b, p.b);
  }
  return a <= b ? [a, b] : null;
}
