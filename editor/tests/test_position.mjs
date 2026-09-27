import assert from "node:assert/strict";
import { fillGap } from "../static/js/bounds.js";
import { Project } from "../static/js/project.js";
import { ResultStore } from "../static/js/results.js";

// The subject's center is pushed by the mean of its trackers' motions, not
// averaged from their absolute positions. See editor/PLAN.md §2.

const near = (a, b, tol, msg) => assert.ok(Math.abs(a - b) <= tol, `${msg}: ${a} vs ${b} (±${tol})`);
const N = 200;

const project = new Project(N, 1920, 1080);
const results = new ResultStore();
const s = project.addSubject("Hero");

// 5 points at frame 0, all moving +1 px/frame. First position = their mean (20).
const points = [];
for (let i = 0; i < 5; i++) points.push(project.addPoint(s.id, 0, 10 * i, 100));
const track = (p, fn, vis = 1) => {
  const key = project.segments(p)[0].key;
  const data = new Float32Array(N * 3);
  for (let f = 0; f < N; f++) {
    const [x, y] = fn(f);
    data[3 * f] = x;
    data[3 * f + 1] = y;
    data[3 * f + 2] = vis;
  }
  results.write(key, 0, 0, data);
};
const moving = (f) => [10 * points.indexOf(p) + f, 100];
for (const p of points) track(p, (f) => [p.keys[0].x + f, 100]);

const at = (f) => project.subjectState(s, f, results);
near(at(0).rawX, 20, 1e-4, "first position is the mean of the starting trackers");
near(at(10).rawX, 30, 1e-4, "pushed by the mean motion");
near(at(49).rawX, 69, 1e-4, "before the removal");

// Remove two trackers at frame 50: the subject must not jump to the new mean (80).
project.endTrackerAt(points[0].id, 50);
project.endTrackerAt(points[1].id, 50);
near(at(50).rawX, 70, 1e-4, "removing trackers does not jump to the new mean");
near(at(50).x, 70, 1e-4, "the final position follows");
near(at(60).rawX, 80, 1e-4, "the remaining trackers keep pushing");

// A manual adjustment: offset key at 60, then the push continues from it.
project.setOffsetKey(s.id, 60, 100, 0);
near(at(60).x, 180, 1e-4, "manual adjustment moves the subject");
near(at(61).x, 181, 1e-4, "the shift is kept and the subject keeps being pushed");
near(at(61).rawX, 81, 1e-4, "the pushed position itself is untouched by the offset");

// A new tracker joining does not yank the subject: it pushes from its second frame.
const late = project.addPoint(s.id, 160, 900, 900);
track(late, (f) => [900 + (f - 160), 900]);
near(at(159).rawX, 179, 1e-4, "before the new tracker");
near(at(160).rawX, 180, 1e-4, "a new tracker does not pull the position");
near(at(161).rawX, 181, 1e-4, "and pushes from its second frame");

// Frames where everything has drifted hold the position. A drifted tracker's
// motion reference is reset when it returns (its accumulated rejected
// displacement must not be injected); pushing resumes from the return frame.
const bounds = project.ensureBounds(s.id);
for (let f = 100; f <= 120; f++) bounds.set(f, [5000, 5000, 40, 40]);
project.touch();
near(at(99).rawX, 119, 1e-4, "before the gap");
assert.equal(at(110).rawX, undefined, "no position while everything has drifted");
assert.equal(at(110).status, "lost", "status while everything has drifted");
near(at(121).rawX, 119, 1e-4, "a drifted tracker's gap motion is not injected on return");
near(at(122).rawX, 120, 1e-4, "pushing resumes from the return frame");
near(at(122).x, 120 + 100, 1e-4, "offset still applies");

// All trackers end; a tracker created later re-anchors the position to it.
for (const p of [...points.slice(2), late]) project.endTrackerAt(p.id, 150);
const fresh = project.addPoint(s.id, 180, 500, 300);
track(fresh, (f) => [500 + 2 * (f - 180), 300 + (f - 180)]);
near(at(180).rawX, 500, 1e-4, "a fresh tracker re-anchors when nothing else is left");
near(at(181).rawX, 502, 1e-4, "and then pushes");

// Results changes are reflected (no stale cache).
track(fresh, (f) => [500 + 2 * (f - 180) + (f >= 199 ? 60 : 0), 300 + (f - 180)]);
near(at(198).rawX, 536, 1e-4, "before the jump");
near(at(199).rawX, 536 + 62, 1e-4, "rewritten results are picked up");

// Hidden CoTracker points still push (v1 decision), and a subject with no
// trackers has no position at all.
const s2 = project.addSubject("Empty");
assert.equal(project.subjectState(s2, 0, results).status, "none");
assert.equal(project.subjectState(s2, 0, results).x, undefined);
const s3 = project.addSubject("Ghost");
const g1 = project.addPoint(s3.id, 0, 50, 50);
track(g1, (f) => [50 + f, 50], 0.1);
near(project.subjectState(s3, 5, results).rawX, 55, 1e-4, "hidden points still push");
near(project.subjectState(s3, 5, results).hidden, 1, 1e-4, "and are counted as hidden");

// Bounds are display/guide only for the pushed path: a box around the trackers
// changes nothing (and the cache rebuild after the edit must agree).
fillGap(bounds, 0, [30, 100, 100, 100], N);
project.touch();
near(at(10).rawX, 30, 1e-4, "bounds do not change the pushed position");
assert.equal(at(10).status, "ok");

// ---- the incremental cache must agree with a from-scratch integration ----------
//
// Edits record the first frame they can affect, and the cache resumes from a
// checkpoint just before it. Every result must be identical to integrating the
// same model from frame 0 in a fresh project.
const M = 1000;
const P = new Project(M, 1920, 1080);
const R = new ResultStore();
const sub = P.addSubject("Hero");
const a = P.addPoint(sub.id, 0, 200, 200);
const b = P.addPoint(sub.id, 0, 240, 200);
const tpl = P.addTemplate(sub.id, { f: 50, x: 500, y: 400, w: 20, h: 20, hx: 10, hy: 10, mask: "" });
const move = (p, x0, y0, n = M, f0 = 0, dx = 1.5) => {
  const key = P.segments(p)[0].key;
  const data = new Float32Array((n - f0) * 3);
  for (let f = f0; f < n; f++) {
    data[3 * (f - f0)] = x0 + dx * f;
    data[3 * (f - f0) + 1] = y0 + 0.5 * f;
    data[3 * (f - f0) + 2] = p.kind === "template" ? 1.5 : 1; // template: look 0, score 0.5
  }
  R.write(key, p.start, f0, data);
};
move(a, 200, 200);
move(b, 240, 200);
move(tpl, 500, 400);

const snap = () => {
  const q = new Project(M, P.width, P.height);
  q.load(JSON.parse(JSON.stringify(P.toJSON())));
  const r = new ResultStore();
  const keys = [];
  for (const t of P.trackers) for (const sg of P.segments(t)) keys.push(sg.key);
  r.load(JSON.parse(JSON.stringify(R.serialize(keys))));
  return { q, r, s: q.subjects[0] };
};
const sample = (p, r, s) => JSON.stringify(Array.from({ length: 100 }, (_, i) => p.baseAt(s, i * 10, r) ?? null));
const agree = (label) => {
  const { q, r, s } = snap();
  assert.equal(sample(P, R, P.subjects[0]), sample(q, r, s), `cache matches a fresh integration after ${label}`);
};

// Warm the cache up to the end so checkpoints exist.
assert.ok(P.baseAt(sub, M - 1, R));
agree("the initial integration");

R.write(P.segments(a)[0].key, 0, 500, new Float32Array([0, 0, 1, 0, 0, 1, 0, 0, 1]));
agree("a results rewrite in the middle");
P.setKey(b.id, 700, 9999, 9999);
agree("a tracker drag");
P.setKey(b.id, 700, 200 + 0.5 * 700, 200 + 0.5 * 700);
agree("a drag back");
P.endTrackerAt(a.id, 620);
agree("ending a tracker");
P.clearEnd(a.id);
agree("clearing the end");
P.endTrackerAt(a.id, 620);
P.setTrackerEnd(a.id, 700);
agree("moving an end later (newly active frames)");
P.clearEnd(a.id);
agree("clearing the moved end");
P.toggleManualAt(b.id, 300, null);
agree("manual on");
P.setKey(b.id, 400, 800, 300);
agree("a manual key");
P.toggleManualAt(b.id, 450, null);
agree("manual off");
const late2 = P.addPoint(sub.id, 800, 100, 100);
agree("a new tracker");
P.endTrackerAt(late2.id, 900);
agree("ending the new tracker");
R.truncate(P.segments(tpl)[0].key, 400);
agree("a results truncation");
P.deleteKey(b.id, 400);
agree("deleting a key");
const bd = P.ensureBounds(sub.id);
fillGap(bd, 0, [0, 0, 60, 60], M);
P.touch(0);
agree("bounds that make everything drift");
fillGap(bd, 0, [300, 300, 4000, 4000], M);
P.touch(0);
agree("bounds that contain everything");
P.setOffsetKey(sub.id, 100, 5, 5);
agree("an offset key");
P.removeLook(tpl.id, tpl.looks[0].id);
assert.equal(P.trackers.length, 4); // a, b, the template (removing its only look is refused) and the late point
assert.equal(tpl.looks.length, 1);
console.log("Position cache: PASS");

console.log("Position: PASS");