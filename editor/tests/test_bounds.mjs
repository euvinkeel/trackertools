import assert from "node:assert/strict";
import { clearBounds, falloff, fillGap, insideBounds, nudge, synthesize } from "../static/js/bounds.js";
import { b64ToF32, ChunkStore } from "../static/js/chunks.js";
import { Project } from "../static/js/project.js";
import { ResultStore } from "../static/js/results.js";

const near = (a, b, tol, msg) => assert.ok(Math.abs(a - b) <= tol, `${msg}: ${a} vs ${b} (±${tol})`);

// ---- falloff / nudge / gap fill / merge ---------------------------------------------------
assert.equal(falloff(0, 10), 1);
assert.equal(falloff(10, 10), 0);
near(falloff(5, 10), 0.5, 1e-9, "falloff midpoint");
assert.equal(falloff(-3, 10), falloff(3, 10));

let st = new ChunkStore(4);
assert.deepEqual(fillGap(st, 30, [100, 100, 50, 40], 100), [0, 99]);
assert.deepEqual(st.get(99), [100, 100, 50, 40]);
assert.deepEqual(nudge(st, 50, [10, 0, 0, 0], 10, 100), [41, 59]);
near(st.get(50)[0], 110, 1e-4, "nudge full on the frame");
near(st.get(45)[0], 105, 1e-4, "nudge half at R/2");
assert.equal(st.get(40)[0], 100);
nudge(st, 0, [0, 0, -1000, 0], 1, 100);
assert.equal(st.get(0)[2], 8, "size is clamped");
assert.deepEqual(clearBounds(st, 40, 59), [40, 59]);
assert.equal(st.get(50), null);
assert.equal(nudge(st, 50, [5, 0, 0, 0], 3, 100), null, "nudge only touches frames with bounds");
assert.deepEqual(fillGap(st, 50, [0, 0, 20, 20], 100), [40, 59]);
assert.equal(fillGap(st, 50, [0, 0, 20, 20], 100), null);
st = new ChunkStore(4);
fillGap(st, 0, [0, 0, 20, 20], 100);
assert.ok(insideBounds([100, 100, 50, 50], 129, 100) && !insideBounds([100, 100, 50, 50], 131, 100));

// ---- Puppeteer synthesis on synthetic mouse traces --------------------------------------
const fps = 60;
const rate = 0.5;
const lagFrames = 0.25 * rate * fps; // hand trails the subject by 0.25 s of real time

function trace(n, subject, jiggle = () => [0, 0]) {
  const out = [];
  for (let u = 0; u < n; u += 0.5) {
    const [sx, sy] = subject(Math.max(0, u - lagFrames));
    const [jx, jy] = jiggle(u / fps / rate);
    out.push({ u, x: sx + jx, y: sy + jy });
  }
  return out;
}

// Lag compensation: with the motion window disabled the center follows the subject.
const linear = (f) => [200 + 5 * f, 300];
let boxes = synthesize(trace(600, linear), { fps, a: 0, b: 599, rate, before: 0, after: 0 });
let worst = 0;
for (let f = 30; f < 560; f++) worst = Math.max(worst, Math.abs(boxes[4 * f] - linear(f)[0]));
assert.ok(worst < 1, `lag-compensated center error ${worst}`);
const noComp = synthesize(trace(600, linear), { fps, a: 0, b: 599, rate, before: 0, after: 0, lag: 0 });
near(noComp[4 * 300], linear(300)[0] - 5 * lagFrames, 1, "without compensation the box trails");

// Jiggle sets the size.
const still = () => [960, 540];
const size = (A) => {
  const b = synthesize(trace(600, still, (t) => [A * Math.sin(2 * Math.PI * 4 * t), A * Math.cos(2 * Math.PI * 4 * t)]),
    { fps, a: 0, b: 599, rate });
  return b[4 * 300 + 2];
};
near(size(0), 32, 0.5, "still mouse gives the minimum box");
const w20 = size(20);
const w40 = size(40);
assert.ok(w20 > 70 && w20 < 110, `jiggle 20 px -> width ${w20}`);
assert.ok(w40 > 1.7 * w20 - 30 && w40 < 2.2 * w20, `jiggle 40 px -> width ${w40} (20 px: ${w20})`);

// Inclusive of motion: an erratic path stays inside the box.
let seed = 7;
const rand = () => ((seed = (seed * 16807) % 2147483647) / 2147483647) - 0.5;
const path = [[800, 500]];
let vx = 0, vy = 0;
for (let f = 1; f < 900; f++) {
  if (f % 20 === 0) { vx = rand() * 24; vy = rand() * 24; }
  const [x, y] = path[f - 1];
  path.push([x + vx, y + vy]);
}
const erratic = (u) => {
  const f = Math.min(899, Math.floor(u));
  const t = u - f;
  const g = Math.min(899, f + 1);
  return [path[f][0] + (path[g][0] - path[f][0]) * t, path[f][1] + (path[g][1] - path[f][1]) * t];
};
boxes = synthesize(trace(900, erratic, (t) => [6 * Math.sin(2 * Math.PI * 5 * t), 6 * Math.cos(2 * Math.PI * 3 * t)]),
  { fps, a: 0, b: 899, rate, width: 1920, height: 1080 });
let inside = 0;
for (let f = 0; f < 900; f++) if (insideBounds(boxes.subarray(4 * f, 4 * f + 4), ...path[f], 0)) inside++;
assert.ok(inside / 900 >= 0.97, `erratic path inside the box on ${inside}/900 frames`);

// ---- model: drift flags, subject counts, run payload ----------------------------------------
const project = new Project(100, 1920, 1080);
const results = new ResultStore();
const s = project.addSubject("Hero");
const p1 = project.addPoint(s.id, 0, 100, 100);
const p2 = project.addPoint(s.id, 0, 120, 100);
const k1 = project.segments(p1)[0].key;
const k2 = project.segments(p2)[0].key;
const data = (fn) => Array.from({ length: 100 }, (_, i) => fn(i)).flat();
results.write(k1, 0, 0, data(() => [100, 100, 1]));
results.write(k2, 0, 0, data((f) => [120 + (f >= 50 ? 400 : 0), 100, 1]));
assert.equal(project.subjectState(s, 60, results).n, 2, "no bounds: nothing drifts");
fillGap(project.ensureBounds(s.id), 0, [110, 100, 100, 100], 100);
project.touch();
assert.equal(project.trackerState(p2, 60, results).drifted, true);
assert.equal(project.trackerState(p2, 40, results).drifted, false);
const ss = project.subjectState(s, 60, results);
assert.equal(ss.n, 1);
assert.equal(ss.drifted, 1);
// The subject keeps the position its trackers pushed it to: p2's jump doesn't
// move it, and p1 has been still, so it stays at the starting mean (110).
near(ss.x, 110, 1e-6, "a drifted tracker's jump doesn't move the subject");
assert.equal(project.driftedRunStart(p2, 70, results), 50);
const plan = project.planRun(0, new ResultStore());
assert.equal(plan.segments.length, 2);
const b = plan.bounds[s.id];
assert.equal(b.f0, 0);
assert.equal(b.guide, true);
assert.deepEqual(Array.from(b64ToF32(b.data).subarray(4 * 99, 4 * 100)), [110, 100, 100, 100]);
// Resuming skips drifted frames: p2's last good on-bounds frame is 49.
results.truncate(k2, 80);
const resume = project.planRun(0, results).segments.find((x) => x.key === k2);
assert.equal(resume.q, 49);
// Truncate patches restore exactly.
const patch = results.truncate(k1, 10);
assert.equal(results.hi(k1), 9);
results.restore([patch]);
assert.equal(results.hi(k1), 99);
assert.deepEqual(results.get(k1, 50), [100, 100, 1]);
// Undo snapshot covers bounds (copy-on-write chunks).
const snap = project.snapshot();
nudge(project.boundsStore(s), 20, [50, 0, 0, 0], 5, 100);
assert.equal(project.sameAs(snap), false);
project.restore(snap);
assert.deepEqual(project.boundsAt(project.subject(s.id), 20), [110, 100, 100, 100]);

console.log("Bounds: PASS");
