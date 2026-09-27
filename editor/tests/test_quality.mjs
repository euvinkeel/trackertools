import assert from "node:assert/strict";
import { fillGap } from "../static/js/bounds.js";
import { Project } from "../static/js/project.js";
import { applyAutoEnds, confirmationFrames, scanAutoEnds } from "../static/js/quality.js";
import { ResultStore } from "../static/js/results.js";

// Deterministic quality analysis: sustained escapes become reversible automatic
// ends, and confirmed motion outliers can be excluded from pushing. See
// editor/PLAN.md §5.

const near = (a, b, tol, msg) => assert.ok(Math.abs(a - b) <= tol, `${msg}: ${a} vs ${b} (±${tol})`);
assert.equal(confirmationFrames(60, 0.1), 6);
assert.equal(confirmationFrames(30, 0.1), 3);
assert.equal(confirmationFrames(30, 0.001), 3);

const N = 200;
const P = new Project(N, 1920, 1080, 30);
const R = new ResultStore();
const s = P.addSubject("Hero");
const bounds = P.ensureBounds(s.id);
fillGap(bounds, 0, [500, 500, 120, 120], N);
P.touch();

const track = (p, fn) => {
  const key = P.segments(p)[0].key;
  const data = new Float32Array(N * 3);
  for (let f = 0; f < N; f++) {
    const [x, y] = fn(f);
    data[3 * f] = x;
    data[3 * f + 1] = y;
    data[3 * f + 2] = 1;
  }
  R.write(key, 0, 0, data);
  return key;
};

const coherent = [];
for (let i = 0; i < 4; i++) coherent.push(P.addPoint(s.id, 0, 450 + i * 20, 500));
const stray = P.addPoint(s.id, 0, 470, 530);
for (const p of coherent) track(p, () => [p.keys[0].x, 500]);
track(stray, (f) => (f < 50 ? [470, 530] : [800, 530]));

// Flag-only policy: nothing is ended automatically.
assert.equal(scanAutoEnds(P, R, s, { fps: 30 }).size, 0);
P.updateSubject(s.id, { driftPolicy: "end" });
const candidates = scanAutoEnds(P, R, s, { fps: 30 });
assert.deepEqual([...candidates.keys()], [stray.id]);
assert.equal(candidates.get(stray.id).f, 50, "the end lands on the first escaped frame");
assert.equal(candidates.get(stray.id).confirmedAt, 52, "confirmed after 3 frames at 30 fps");
assert.equal(candidates.get(stray.id).reason, "bounds");

const changed = P.recomputeAutoEnds(s.id, R);
assert.deepEqual(changed.map((p) => p.id), [stray.id]);
assert.equal(stray.autoEnd.f, 50);
assert.equal(P.end(stray), 50);
assert.equal(P.trackerState(stray, 49, R).mode, "auto");
assert.equal(P.trackerState(stray, 50, R), null);
// A user removal turns automatic ending off for that tracker.
P.clearEnd(stray.id);
assert.equal(stray.noAutoEnd, true);
assert.deepEqual(P.recomputeAutoEnds(s.id, R), []);
assert.equal(stray.autoEnd, undefined);
// Re-enabling restores it on the next analysis.
P.setAutoEnd(stray.id, true);
P.recomputeAutoEnds(s.id, R);
assert.equal(stray.autoEnd.f, 50);
// Moving the bounds so the stray is inside clears the automatic end.
for (let f = 0; f < N; f++) bounds.set(f, [600, 600, 2000, 2000]);
P.touch();
P.recomputeAutoEnds(s.id, R);
assert.equal(stray.autoEnd, undefined);
assert.equal(P.trackerState(stray, 100, R).mode, "auto");

// Motion exclusion: a one-frame jump is not a permanent end, but it is kept
// out of the pushed position when the subject asks for exclusion.
const Q = new Project(N, 1920, 1080, 30);
const RQ = new ResultStore();
const q = Q.addSubject("Cohort");
const pts = [];
for (let i = 0; i < 5; i++) pts.push(Q.addPoint(q.id, 0, 200 + i * 40, 300));
const keys = pts.map((p, i) => {
  const key = Q.segments(p)[0].key;
  const data = new Float32Array(N * 3);
  for (let f = 0; f < N; f++) {
    const jump = i === 4 && f === 30 ? 100 : 0;
    data[3 * f] = 200 + i * 40 + f + jump;
    data[3 * f + 1] = 300;
    data[3 * f + 2] = 1;
  }
  RQ.write(key, 0, 0, data);
  return key;
});
const before = Q.subjectState(q, 29, RQ).rawX;
assert.equal(Q.subjectState(q, 30, RQ).outliers, 0, "no policy: no outlier analysis");
assert.equal(Q.subjectState(q, 30, RQ).rawX, before + 21, "the jump is averaged in without a policy");
Q.updateSubject(q.id, { motionPolicy: "exclude" });
const st = Q.subjectState(q, 30, RQ);
assert.equal(st.outliers, 1);
near(st.rawX, before + 1, 1e-6, "the outlier is excluded from pushing");
assert.equal(Q.subjectState(q, 31, RQ).outliers, 1, "its return jump is excluded too");
near(Q.subjectState(q, 32, RQ).rawX, before + 3, 1e-6, "the coherent dots keep pushing");
// A one-frame jump does not become an automatic end.
Q.updateSubject(q.id, { motionPolicy: "end" });
assert.deepEqual(applyAutoEnds(Q, RQ, q, { fps: 30 }), []);
assert.equal(pts[4].autoEnd, undefined);

console.log("Quality analysis: PASS");
