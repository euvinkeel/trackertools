import assert from "node:assert/strict";
import { analyzeMotion, applyModel, combineAnalyses, fitSimilarity, modelScale } from "../static/js/motion.js";

// Motion-consistency analysis: the group's translation/rotation/zoom is fitted
// and a straggler is found by its residual, even though every dot's own motion
// vector differs. See editor/PLAN.md §2.

const near = (a, b, tol, msg) => assert.ok(Math.abs(a - b) <= tol, `${msg}: ${a} vs ${b} (±${tol})`);
const P = (x, y, ok = true) => ({ x, y, ok });

function grid(n = 8, step = 40) {
  const out = [];
  for (let i = 0; i < n; i++) out.push(P(100 + (i % 4) * step, 200 + Math.floor(i / 4) * step));
  return out;
}

// Translation.
let prev = grid();
let cur = prev.map((p) => P(p.x + 5, p.y + 3));
let r = analyzeMotion(prev, cur);
assert.equal(r.ok, true);
assert.equal(r.outliers.length, 0);
near(r.scale, 1, 1e-6, "translation scale");
near(r.rotation, 0, 1e-6, "translation rotation");
near(r.model.tx, 5, 1e-6, "translation tx");

// Zoom about an arbitrary center: every dot's motion vector is different.
const zoom = (p, s, cx = 500, cy = 400) => P(cx + (p.x - cx) * s, cy + (p.y - cy) * s);
cur = prev.map((p) => zoom(p, 1.25));
r = analyzeMotion(prev, cur);
assert.equal(r.ok, true);
assert.equal(r.outliers.length, 0);
near(r.scale, 1.25, 1e-4, "zoom scale");

// Rotation.
const rot = (p, deg, cx = 500, cy = 400) => {
  const a = (deg * Math.PI) / 180;
  const dx = p.x - cx;
  const dy = p.y - cy;
  return P(cx + dx * Math.cos(a) - dy * Math.sin(a), cy + dx * Math.sin(a) + dy * Math.cos(a));
};
cur = prev.map((p) => rot(p, 30));
r = analyzeMotion(prev, cur);
assert.equal(r.ok, true);
assert.equal(r.outliers.length, 0);
near((r.rotation * 180) / Math.PI, 30, 1e-3, "rotation angle");

// Translation + zoom + rotation together.
cur = prev.map((p) => { const q = zoom(p, 1.1); return rot(q, -12); });
r = analyzeMotion(prev, cur);
assert.equal(r.ok, true);
assert.equal(r.outliers.length, 0);
near(r.scale, 1.1, 1e-3, "combined scale");
near((r.rotation * 180) / Math.PI, -12, 1e-2, "combined rotation");

// One straggler: identify it while retaining the seven coherent dots.
cur = prev.map((p, i) => (i === 5 ? P(p.x + 120, p.y - 90) : zoom(p, 1.15)));
r = analyzeMotion(prev, cur);
assert.equal(r.ok, true);
assert.deepEqual(r.outliers, [5]);
assert.equal(r.inliers.length, 7);
assert.ok(r.residuals.get(5) > r.threshold, "the straggler exceeds the threshold");

// Spatially collinear but well-separated dots still fit (the line's direction
// and length determine the similarity).
prev = [];
for (let i = 0; i < 8; i++) prev.push(P(100 + i * 80, 300));
cur = prev.map((p) => P(100 + (p.x - 100) * 1.05, 300));
r = analyzeMotion(prev, cur);
assert.equal(r.ok, true);
assert.equal(r.outliers.length, 0);
near(r.scale, 1.05, 1e-4, "collinear scale");

// Insufficient evidence: too few dots.
r = analyzeMotion(grid(3), grid(3).map((p) => P(p.x + 2, p.y)));
assert.equal(r.ok, false);
assert.match(r.reason, /few/);

// Insufficient evidence: a tiny cluster.
const close = [];
for (let i = 0; i < 8; i++) close.push(P(500 + (i % 3), 400 + Math.floor(i / 3)));
r = analyzeMotion(close, close.map((p) => P(p.x + 1, p.y)));
assert.equal(r.ok, false);
assert.match(r.reason, /close/);

// Insufficient evidence: a 4-vs-4 split has no convincing majority.
prev = grid();
cur = prev.map((p, i) => P(p.x + (i < 4 ? 10 : -10), p.y));
r = analyzeMotion(prev, cur);
assert.equal(r.ok, false);
assert.match(r.reason, /consensus/);

// Slow accumulated drift: invisible frame-to-frame, clear over 0.25 s.
prev = grid();
cur = prev.map((p, i) => P(p.x + (i === 3 ? 21 : 15), p.y + 15));
const adjacent = analyzeMotion(grid().map((p) => P(p.x + 1, p.y + 1)), grid().map((p, i) => P(p.x + 1 + (i === 3 ? 0.4 : 0), p.y + 1)));
const long = analyzeMotion(prev, cur);
assert.equal(adjacent.ok, true);
assert.equal(long.ok, true);
assert.deepEqual(adjacent.outliers, [], "a 0.4 px/frame drift is below the floor on one frame");
assert.deepEqual(long.outliers, [3], "but it accumulates over the long interval");
assert.deepEqual(combineAnalyses(adjacent, long).outliers, [3], "the combined analysis reports it");

// fitSimilarity agrees with the model evaluation.
const model = { a: 1, b: 0.5, tx: 10, ty: 20 };
const pairs = [[0, 0], [100, 0], [0, 100]].map(([x, y]) => {
  const [u, v] = applyModel(model, x, y);
  return [x, y, u, v];
});
const m = fitSimilarity(pairs);
const [mx, my] = applyModel(m, 50, 50);
near(mx, 10 + 50 - 25, 1e-6, "refit x");
near(my, 20 + 25 + 50, 1e-6, "refit y");
near(modelScale(m), Math.hypot(1, 0.5), 1e-6, "refit scale");

console.log("Motion analysis: PASS");
