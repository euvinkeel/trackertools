// Subject bounds: a rough animated box per subject, stored as a ChunkStore with
// channels [cx, cy, w, h] (source pixels, NaN = no bounds on that frame).
// Everything here is pure (no DOM) so it can be unit-tested in Node.
// See editor/PLAN.md §6.

export const BOUNDS_CHANNELS = 4;
export const DRIFT_MARGIN = 0.1; // of the box size, added on every side
export const MIN_SIZE = 8;

export function insideBounds(b, x, y, margin = DRIFT_MARGIN) {
  return Math.abs(x - b[0]) <= b[2] * (0.5 + margin) && Math.abs(y - b[1]) <= b[3] * (0.5 + margin);
}

// Raised cosine: 1 at d = 0, falling smoothly to 0 at |d| = R.
export function falloff(d, R) {
  if (R <= 0) return d === 0 ? 1 : 0;
  const t = Math.abs(d) / R;
  return t >= 1 ? 0 : 0.5 * (1 + Math.cos(Math.PI * t));
}

function clampSize(v) {
  v[2] = Math.max(MIN_SIZE, v[2]);
  v[3] = Math.max(MIN_SIZE, v[3]);
  return v;
}

// Add delta = [dcx, dcy, dw, dh] fully on frame f, fading out over ±R frames.
// Only frames that already have bounds change. Returns the changed [a, b] or null.
export function nudge(store, f, delta, R, frameCount) {
  let a = -1;
  let b = -1;
  const r = Math.max(1, Math.round(R));
  for (let g = Math.max(0, f - r + 1); g <= Math.min(frameCount - 1, f + r - 1); g++) {
    const w = falloff(g - f, r);
    if (!w) continue;
    const v = store.get(g);
    if (!v) continue;
    for (let c = 0; c < 4; c++) v[c] += delta[c] * w;
    store.set(g, clampSize(v));
    if (a < 0) a = g;
    b = g;
  }
  return a < 0 ? null : [a, b];
}

// Fill the undefined gap around frame f with a constant box (the whole video
// when there are no bounds yet). Returns [a, b] or null if f already has bounds.
export function fillGap(store, f, box, frameCount) {
  if (store.has(f)) return null;
  const prev = f > 0 ? store.prevDefined(f - 1) : -1;
  const next = f + 1 < frameCount ? store.nextDefined(f + 1) : -1;
  const a = prev + 1;
  const b = next < 0 ? frameCount - 1 : next - 1;
  const v = clampSize(box.slice());
  for (let g = a; g <= b; g++) store.set(g, v);
  return [a, b];
}

// Clear frames [a, b]. Returns the range actually cleared, or null.
export function clearBounds(store, a, b) {
  const first = store.nextDefined(Math.max(0, a));
  if (first < 0 || first > b) return null;
  const last = store.prevDefined(b);
  store.clearRange(first, last + 1);
  return [first, last];
}

// ---- Puppeteer synthesis ---------------------------------------------------------------
// Time windows are in real seconds and get converted to video frames with the
// playback rate: the hand's lag and jiggle happen in real time.
export const PUPPETEER_DEFAULTS = {
  rate: 0.5, // playback rate during the pass
  lag: 0.25, // how far the hand trails the subject
  sigmaCenter: 0.12, // smoothing of the followed position
  sigmaSize: 0.25, // window of the jiggle measurement
  gain: 1, // jiggle -> size multiplier
  pad: 12, // px added to every half-size
  minHalf: 16, // px
  before: 0.1, // box includes motion from this long before...
  after: 0.3, // ...to this long after each frame
  smoothPos: 0.08,
  smoothSize: 0.2,
};

export function gaussSmooth(values, sigma) {
  const n = values.length;
  if (sigma < 0.3 || n < 2) return Float64Array.from(values);
  const r = Math.ceil(sigma * 3);
  const k = new Float64Array(2 * r + 1);
  for (let i = -r; i <= r; i++) k[i + r] = Math.exp((-i * i) / (2 * sigma * sigma));
  const out = new Float64Array(n);
  for (let i = 0; i < n; i++) {
    let s = 0;
    let ws = 0;
    for (let j = Math.max(0, i - r); j <= Math.min(n - 1, i + r); j++) {
      const w = k[j - i + r];
      s += values[j] * w;
      ws += w;
    }
    out[i] = s / ws;
  }
  return out;
}

// Linear interpolation of sorted samples [{u, x, y}] at position u (held at the ends).
export function sampleAt(samples, u) {
  const n = samples.length;
  if (u <= samples[0].u) return [samples[0].x, samples[0].y];
  if (u >= samples[n - 1].u) return [samples[n - 1].x, samples[n - 1].y];
  let lo = 0;
  let hi = n - 1;
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1;
    if (samples[mid].u <= u) lo = mid;
    else hi = mid;
  }
  const A = samples[lo];
  const B = samples[hi];
  const t = B.u > A.u ? (u - A.u) / (B.u - A.u) : 0;
  return [A.x + (B.x - A.x) * t, A.y + (B.y - A.y) * t];
}

// samples: pointer positions in source pixels with u = continuous video frame
// index, sorted by u. Returns boxes (Float32Array, 4 per frame) for frames [a, b].
export function synthesize(samples, { fps, a, b, width = Infinity, height = Infinity, ...options }) {
  const o = { ...PUPPETEER_DEFAULTS, ...options };
  const n = b - a + 1;
  if (!samples.length || n <= 0) return new Float32Array(0);
  const frames = (sec) => sec * o.rate * fps;
  const lag = frames(o.lag);
  const px = new Float64Array(n);
  const py = new Float64Array(n);
  for (let i = 0; i < n; i++) [px[i], py[i]] = sampleAt(samples, a + i + lag);

  const cx = gaussSmooth(px, frames(o.sigmaCenter));
  const cy = gaussSmooth(py, frames(o.sigmaCenter));
  const dx2 = gaussSmooth(px.map((v, i) => (v - cx[i]) ** 2), frames(o.sigmaSize));
  const dy2 = gaussSmooth(py.map((v, i) => (v - cy[i]) ** 2), frames(o.sigmaSize));
  const half = (m2, cap) => Math.min(cap, Math.max(o.minHalf, o.gain * 2.2 * Math.sqrt(m2) + o.pad));

  // Union of the boxes over [f - before, f + after].
  const lo = Math.round(frames(o.before));
  const hi = Math.round(frames(o.after));
  const x0 = new Float64Array(n);
  const x1 = new Float64Array(n);
  const y0 = new Float64Array(n);
  const y1 = new Float64Array(n);
  for (let i = 0; i < n; i++) {
    let ax = Infinity, bx = -Infinity, ay = Infinity, by = -Infinity;
    for (let j = Math.max(0, i - lo); j <= Math.min(n - 1, i + hi); j++) {
      const hx = half(dx2[j], width);
      const hy = half(dy2[j], height);
      ax = Math.min(ax, cx[j] - hx);
      bx = Math.max(bx, cx[j] + hx);
      ay = Math.min(ay, cy[j] - hy);
      by = Math.max(by, cy[j] + hy);
    }
    x0[i] = ax; x1[i] = bx; y0[i] = ay; y1[i] = by;
  }
  const fcx = gaussSmooth(x0.map((v, i) => (v + x1[i]) / 2), frames(o.smoothPos));
  const fcy = gaussSmooth(y0.map((v, i) => (v + y1[i]) / 2), frames(o.smoothPos));
  const fw = gaussSmooth(x0.map((v, i) => x1[i] - v), frames(o.smoothSize));
  const fh = gaussSmooth(y0.map((v, i) => y1[i] - v), frames(o.smoothSize));
  const out = new Float32Array(n * 4);
  for (let i = 0; i < n; i++) {
    out[4 * i] = fcx[i];
    out[4 * i + 1] = fcy[i];
    out[4 * i + 2] = Math.max(MIN_SIZE, fw[i]);
    out[4 * i + 3] = Math.max(MIN_SIZE, fh[i]);
  }
  return out;
}

// Float32 [cx, cy, w, h] per frame for frames [a, b] (NaN where undefined), as
// sent to the tracking server.
export function packBounds(store, a, b) {
  const arr = new Float32Array((b - a + 1) * 4).fill(NaN);
  for (let f = store.nextDefined(a); f >= 0 && f <= b; f = store.nextDefined(f + 1)) {
    arr.set(store.get(f), (f - a) * 4);
  }
  return arr;
}
