import assert from "node:assert/strict";
import { Project } from "../static/js/project.js";
import { ResultStore } from "../static/js/results.js";
import { decodeMask, encodeMask, unpackMatch } from "../static/js/templates.js";

const project = new Project(10, 200, 100);
const results = new ResultStore();
const subject = project.addSubject("Cursor");
const look = { f: 0, x: 10, y: 20, w: 9, h: 12, hx: 2.5, hy: 1.5, mask: "" };
const p = project.addTemplate(subject.id, look);
assert.equal(project.trackerState(p, 0, results).x, 12.5);
let key = project.segments(p)[0].key;
assert.equal(key, `${p.id}@0@12.50,21.50`);
assert.equal(unpackMatch(1 * 4 + 0.8 + 1).score.toFixed(1), "0.8");
results.write(key, 0, 0, [12.5, 21.5, 2, 30, 40, 1.9, 31, 41, 1.3, 32, 42, 1.85]);
assert.equal(project.trackerState(p, 1, results).score.toFixed(1), "0.9");
assert.equal(project.trackerState(p, 2, results).lost, true);
assert.equal(project.subjectState(subject, 2, results).status, "lost");
assert.equal(project.subjectState(subject, 2, results).lost, 1);
assert.equal(project.planRun(0, results).segments[0].q, 3); // last found, not last computed
project.setThreshold(p.id, 0.2);
assert.equal(project.segments(p)[0].key, key); // changing threshold retains results
assert.equal(project.trackerState(p, 2, results).lost, false);
assert.equal(project.planRun(0, results).segments[0].q, 3);
project.setThreshold(p.id, 0.7);
results.truncate(key, 2);
assert.equal(results.hi(key), 1);
assert.equal(results.get(key, 2), null);
assert.equal(project.planRun(0, results).segments[0].q, 1);
const before = project.snapshot();
project.addLook(p.id, { ...look, f: 2 });
// Adding a look keeps the key (and the results): it applies from the cursor on,
// and the app truncates from there; frames before it stay tracked.
assert.equal(project.segments(p)[0].key, key);
assert.equal(project.trackerState(p, 1, results).mode, "auto");
assert.equal(project.trackerState(p, 1, results).score.toFixed(1), "0.9");
project.restore(before);
assert.equal(project.segments(project.tracker(p.id))[0].key, key);
const raw = Uint8Array.from({ length: 108 }, (_, i) => i % 3 === 0 ? 1 : 0);
assert.deepEqual(decodeMask(encodeMask(raw), 9, 12), raw);
console.log("Template model: PASS");
