import assert from "node:assert/strict";
const base = "file:///C:/Users/EK/Documents/repos/co-tracker/editor/static/js/";
const { Project } = await import(base + "project.js");
const { ResultStore } = await import(base + "results.js");
const { ChunkStore, CHUNK, evalKeyed } = await import(base + "chunks.js");
globalThis.btoa ??= (s) => Buffer.from(s, "binary").toString("base64");
globalThis.atob ??= (s) => Buffer.from(s, "base64").toString("binary");

const N = 1000;
const P = new Project(N);
const R = new ResultStore();
const s = P.addSubject("Player");
const p = P.addPoint(s.id, 10, 100, 100);
let segs = P.segments(p);
assert.equal(segs.length, 1);
assert.deepEqual([segs[0].q, segs[0].end], [10, N]);
assert.equal(P.trackerState(p, 9, R), null);
assert.equal(P.trackerState(p, 10, R).mode, "auto");
assert.equal(P.trackerState(p, 11, R).mode, "pending");

// simulate tracker output for frames 10..200
const k0 = segs[0].key;
const data = [];
for (let f = 10; f <= 200; f++) data.push(100 + (f - 10), 100, f > 150 ? 0.2 : 0.9);
R.write(k0, 10, 10, data);
assert.equal(R.hi(k0), 200);
let st = P.trackerState(p, 60, R);
assert.equal(st.mode, "auto"); assert.equal(st.x, 150);
assert.equal(P.trackerState(p, 300, R).mode, "pending");
assert.equal(P.trackerState(p, 300, R).x, 290); // ghost at last known

// planRun resumes from last visible frame (150), not hidden 200
let plan = P.planRun(0, R).segments;
assert.equal(plan.length, 1); assert.equal(plan[0].q, 150); assert.equal(plan[0].x, 240);

// re-anchor at 50
P.setKey(p.id, 50, 500, 500);
segs = P.segments(p);
assert.equal(segs.length, 2);
assert.equal(segs[0].key, k0); assert.equal(segs[0].end, 50);
assert.equal(P.trackerState(p, 49, R).x, 139); // old results still valid before anchor
assert.equal(P.trackerState(p, 50, R).x, 500);
assert.equal(P.trackerState(p, 60, R).mode, "pending");

// manual from 30: key at 30 = current pos (120); key at 50 becomes a manual key
const pos30 = P.trackerState(p, 30, R);
assert.equal(P.toggleManualAt(p.id, 30, pos30), "manual");
assert.equal(P.trackerState(p, 40, R).mode, "manual");
assert.equal(P.trackerState(p, 40, R).x, 120 + (500 - 120) * 0.5);
assert.equal(P.trackerState(p, 80, R).x, 500); // hold after last key
assert.equal(P.segments(p).length, 1); // only [10,30)
// toggle off at 45: removes keys >= 45 (the 50 key); auto resumes at 45 from manual pos (hold key 30 -> 120)
assert.equal(P.toggleManualAt(p.id, 45, null), "auto");
segs = P.segments(p);
assert.equal(segs.length, 2);
assert.deepEqual([segs[1].q, segs[1].x, segs[1].end], [45, 120, N]);
assert.equal(P.trackerState(p, 44, R).mode, "manual");
assert.equal(P.trackerState(p, 45, R).mode, "auto");

// manual on at start frame then off at start removes range, keeps creation key
const p2 = P.addPoint(s.id, 100, 10, 10);
P.toggleManualAt(p2.id, 100, { x: 10, y: 10 });
P.setKey(p2.id, 120, 30, 10);
assert.equal(P.segments(p2).length, 0);
P.toggleManualAt(p2.id, 100, null);
assert.equal(P.segments(p2).length, 1);
assert.equal(p2.keys.length, 1);

// subject average includes hidden points
R.write(P.segments(p2)[0].key, 100, 100, [10, 10, 1, 20, 10, 0.1]);
const sub = P.subjectState(s, 101, R);
// p at 101 is pending (seg from 45 has no results) -> partial
assert.equal(sub.status, "partial"); assert.equal(sub.n, 1); assert.equal(sub.hidden, 1);

// end point at 500
P.endTrackerAt(p.id, 500);
assert.equal(P.trackerState(p, 499, R).mode, "pending");
assert.equal(P.trackerState(p, 500, R), null);
assert.equal(P.segments(p).at(-1).end, 500);
// Ending retains later keys and manual ranges: clearing the end restores them.
P.clearEnd(p.id);
P.setKey(p.id, 520, 700, 700);
P.endTrackerAt(p.id, 510);
assert.equal(P.tracker(p.id).keys.some((k) => k.f === 520), true);
assert.equal(P.trackerState(p, 515, R), null);
P.clearEnd(p.id);
assert.equal(P.trackerState(p, 520, R).x, 700);
assert.equal(P.trackerState(p, 520, R).mode, "auto");
// Automatic ends are reversible too, and clearing one suppresses re-adding.
P.autoEndTrackerAt(p.id, 520, "bounds", 526);
assert.equal(P.trackerState(p, 519, R).mode, "pending");
assert.equal(P.trackerState(p, 520, R), null);
assert.equal(P.tracker(p.id).noAutoEnd, undefined);
P.clearEnd(p.id);
assert.equal(P.trackerState(p, 520, R).x, 700);
assert.equal(P.tracker(p.id).noAutoEnd, true);
assert.equal(P.tracker(p.id).autoEnd, undefined);
P.setAutoEnd(p.id, true);
assert.equal(P.tracker(p.id).noAutoEnd, undefined);
// An automatic end at the creation frame keeps the tracker.
P.autoEndTrackerAt(p2.id, 100, "bounds", 100);
assert.equal(P.tracker(p2.id) != null, true);
assert.equal(P.trackerState(P.tracker(p2.id), 100, R), null);
P.clearEnd(p2.id);
// end at start deletes (explicit gesture)
P.endTrackerAt(p2.id, 100);
assert.equal(P.tracker(p2.id), null);
P.endTrackerAt(p.id, 500);
assert.equal(P.subjectState(s, 600, R).status, "none");

// undo snapshot roundtrip and results serialization
const snap = P.snapshot();
P.removeSubject(s.id);
assert.equal(P.trackers.length, 0);
P.restore(snap);
assert.equal(P.trackers.length, 1);
const R2 = new ResultStore();
globalThis.btoa ??= (s) => Buffer.from(s, "binary").toString("base64");
globalThis.atob ??= (s) => Buffer.from(s, "base64").toString("binary");
R2.load(JSON.parse(JSON.stringify(R.serialize([k0]))));
assert.deepEqual(R2.get(k0, 60).map((v) => Math.round(v * 100) / 100), [150, 100, 0.9]);
assert.equal(R2.hi(k0), 200);
// ---- off-frame + subset tracking
const Q = new Project(1000, 1920, 1080);
const RQ = new ResultStore();
const sq = Q.addSubject("A");
const inP = Q.addPoint(sq.id, 0, 100, 100);
const outP = Q.addPoint(sq.id, 0, -50, 200, true); // created off-frame -> manual
assert.equal(Q.segments(outP).length, 0);
assert.equal(Q.trackerState(outP, 10, RQ).mode, "manual");
assert.equal(Q.trackerState(outP, 10, RQ).x, -50);
let pr = Q.planRun(0, RQ);
assert.equal(pr.segments.length, 1); assert.equal(pr.blocked, 0);
// ending manual while off-frame yields a blocked auto segment
Q.toggleManualAt(outP.id, 20, null);
assert.equal(Q.segments(outP)[0].offframe, true);
assert.equal(Q.trackerState(outP, 30, RQ).mode, "blocked");
pr = Q.planRun(0, RQ);
assert.equal(pr.segments.length, 1); assert.equal(pr.blocked, 1);
// subject position counts manual off-frame points, not blocked ones; the
// starting mean (25) is kept as the trackers are pushed from there
const ss = Q.subjectState(sq, 10, RQ);
assert.deepEqual([ss.status, ss.n, ss.x, ss.pending], ["partial", 1, 25, 1]);
assert.equal(Q.subjectState(sq, 30, RQ).status, "unknown"); // pending + blocked only
// subset planning
const p3 = Q.addPoint(sq.id, 0, 500, 500);
pr = Q.planRun(0, RQ, new Set([p3.id]));
assert.deepEqual(pr.segments.map((s) => s.key.split("@")[0]), [String(p3.id)]);
// resume skips frames where the tracked position left the frame
const k3 = Q.segments(p3)[0].key;
RQ.write(k3, 0, 0, [500, 500, 1, 600, 500, 1, 2000, 500, 1]); // frame 2 is off-frame but "visible"
pr = Q.planRun(0, RQ, new Set([p3.id]));
assert.equal(pr.segments[0].q, 1);
// ---- chunk store: values, search, copy-on-write, persistence
const cs = new ChunkStore(2);
cs.set(5, [1, 2]); cs.set(CHUNK * 3 + 7, [3, 4]);
assert.deepEqual(cs.get(5), [1, 2]); assert.equal(cs.get(6), null);
assert.equal(cs.prevDefined(CHUNK * 3), 5); assert.equal(cs.nextDefined(6), CHUNK * 3 + 7);
assert.equal(cs.prevDefined(4), -1); assert.equal(cs.nextDefined(CHUNK * 3 + 8), -1);
assert.deepEqual(cs.extent(), [5, CHUNK * 3 + 7]);
assert.deepEqual(evalKeyed(cs, 0), [1, 2]); // hold before first key
assert.deepEqual(evalKeyed(cs, CHUNK * 10), [3, 4]); // hold after last
const mid = Math.round((5 + CHUNK * 3 + 7) / 2);
assert.ok(Math.abs(evalKeyed(cs, mid)[0] - 2) < 0.01);
const csSnap = cs.snapshot();
cs.set(5, [9, 9]);
assert.deepEqual(ChunkStore.fromSnapshot(csSnap).get(5), [1, 2]); // snapshot untouched
assert.notEqual(csSnap.chunks.get(0), cs.chunks.get(0)); // edited chunk was cloned
assert.equal(csSnap.chunks.get(3), cs.chunks.get(3)); // untouched chunk shared
cs.set(CHUNK * 3 + 7, null);
assert.equal(cs.chunks.has(3), false); // empty chunks are dropped
const cs2 = ChunkStore.deserialize(JSON.parse(JSON.stringify(cs.serialize())));
assert.deepEqual(cs2.get(5), [9, 9]); assert.equal(cs2.counts.get(0), 1);
cs2.clearRange(0, 10);
assert.equal(cs2.empty, true);

// ---- project v2: offset, layers, snapshots with stores, persistence, v1 migration
const V = new Project(500, 1920, 1080);
const RV = new ResultStore();
const sv = V.addSubject("Hero");
V.addPoint(sv.id, 0, 100, 100, true);
V.addPoint(sv.id, 0, 200, 100, true);
assert.equal(V.subjectState(sv, 10, RV).x, 150);
V.setOffsetKey(sv.id, 10, 10, 0);
V.setOffsetKey(sv.id, 20, 30, -10);
assert.deepEqual(V.offsetAt(sv, 0), { dx: 10, dy: 0 });
assert.deepEqual(V.offsetAt(sv, 15), { dx: 20, dy: -5 });
assert.deepEqual(V.offsetAt(sv, 99), { dx: 30, dy: -10 });
let sst = V.subjectState(sv, 15, RV);
assert.deepEqual([sst.rawX, sst.x, sst.y, sst.offX], [150, 170, 95, 20]);
const layerStore = V.newStore(2);
sv.layers.push({ id: V.nextId++, name: "Finetune 1", enabled: true, weight: 0.5, keys: layerStore });
V.store(layerStore).set(15, [4, 2]);
sst = V.subjectState(sv, 15, RV);
assert.deepEqual([sst.fineX, sst.fineY, sst.x], [2, 1, 172]);
const vs = V.snapshot();
assert.equal(V.sameAs(vs), true);
V.store(layerStore).set(16, [8, 8]);
assert.equal(V.sameAs(vs), false); // store edits are detected
V.restore(vs);
assert.equal(V.store(layerStore).get(16), null);
assert.equal(V.sameAs(vs), true);
V.deleteOffsetKey(sv.id, 10);
assert.equal(V.sameAs(vs), false);
const saved = JSON.parse(JSON.stringify(V.toJSON()));
const V2 = new Project(500, 1920, 1080);
V2.load(saved);
assert.deepEqual(V2.store(layerStore).get(15), [4, 2]);
assert.equal(V2.subjectState(V2.subjects[0], 15, RV).x, 150 + 30 + 2);
assert.equal(saved.version, 3);
const V1 = new Project(100);
V1.load({ subjects: [{ id: 1, name: "Old", color: "#22d3ee", hidden: false }],
  points: [{ id: 2, subjectId: 1, start: 0, end: null, keys: [{ f: 0, x: 5, y: 5 }], manual: [] }], nextId: 3 });
assert.equal(V1.trackers[0].kind, "point"); assert.deepEqual(V1.subjects[0].offset, []);
assert.equal(V1.trackersOf(1).length, 1);
// ---- looks no longer re-key results (they apply from the cursor on) ---------
const W = new Project(500, 1920, 1080);
const RW = new ResultStore();
const sw = W.addSubject("T");
const t1 = W.addTemplate(sw.id, { f: 0, x: 100, y: 100, w: 20, h: 20, hx: 10, hy: 10, mask: "" });
const keyBefore = W.segments(t1)[0].key;
RW.write(keyBefore, 0, 0, [110, 110, 1.5, 111, 110, 1.5]);
W.addLook(t1.id, { f: 100, x: 200, y: 200, w: 20, h: 20, hx: 10, hy: 10, mask: "" });
assert.equal(W.segments(t1)[0].key, keyBefore); // the key survives a look change
assert.deepEqual(RW.get(keyBefore, 1), [111, 110, 1.5]); // and so do the results
assert.equal(W.trackerState(t1, 1, RW).mode, "auto");
// Projects saved by older builds stored template results under a look signature;
// loading folds them into the bare key (longest run wins).
const ser = JSON.parse(JSON.stringify(RW.serialize([keyBefore])));
const legacy = {};
legacy[`${keyBefore}#7.2`] = ser[keyBefore];
const RW2 = new ResultStore();
RW2.load(legacy);
assert.equal(RW2.hi(keyBefore), 1);
assert.deepEqual(RW2.get(keyBefore, 1), [111, 110, 1.5]);
const RW3 = new ResultStore();
RW3.load({ [`${keyBefore}#7.2`]: ser[keyBefore], [keyBefore]: { q: 0, hi: -1, chunks: {} } });
assert.equal(RW3.hi(keyBefore), 1); // the richer entry wins
assert.equal(RW3.get(keyBefore, 0)[0], 110);

// ---- finetune layers stack additively (weights, holds, enable) -----------------
const F = new Project(100, 1920, 1080);
const RF = new ResultStore();
const sf = F.addSubject("F");
F.addPoint(sf.id, 0, 100, 100, true);
F.addPoint(sf.id, 0, 200, 100, true);
const l1 = F.newStore(2);
const l2 = F.newStore(2);
sf.layers.push({ id: F.nextId++, name: "A", enabled: true, weight: 0.5, keys: l1 });
sf.layers.push({ id: F.nextId++, name: "B", enabled: true, weight: 2, keys: l2 });
F.store(l1).set(10, [4, 2]);
F.store(l2).set(20, [1, 1]);
// l1 is keyed at 10; l2 holds [1,1] before its first key at 20.
assert.deepEqual([F.subjectState(sf, 10, RF).fineX, F.subjectState(sf, 10, RF).fineY], [4, 3]);
assert.deepEqual([F.subjectState(sf, 20, RF).fineX, F.subjectState(sf, 20, RF).fineY], [4, 3]);
// Both hold outside their keys (before the first and after the last).
assert.deepEqual([F.subjectState(sf, 0, RF).fineX, F.subjectState(sf, 0, RF).fineY], [4, 3]);
assert.deepEqual([F.subjectState(sf, 99, RF).fineX, F.subjectState(sf, 99, RF).fineY], [4, 3]);
sf.layers[0].enabled = false;
assert.deepEqual([F.subjectState(sf, 20, RF).fineX, F.subjectState(sf, 20, RF).fineY], [2, 2]);
sf.layers[0].enabled = true;
sf.layers[1].weight = 0;
assert.deepEqual([F.subjectState(sf, 20, RF).fineX, F.subjectState(sf, 20, RF).fineY], [2, 1]);

console.log("project/results tests passed");
