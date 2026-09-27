import { Project } from "../static/js/project.js";
import { ResultStore } from "../static/js/results.js";

// A 10-minute 60 fps video, 8 CoTracker points tracked end to end.
const N = 36000;
const project = new Project(N, 1920, 1080);
const results = new ResultStore();
const s = project.addSubject("Hero");
const trackers = [];
for (let i = 0; i < 8; i++) trackers.push(project.addPoint(s.id, 0, 500 + i * 40, 400 + i * 20));
const keys = [];
for (const p of trackers) {
  const key = project.segments(p)[0].key;
  keys.push(key);
  const data = new Float32Array(N * 3);
  for (let f = 0; f < N; f++) {
    data[3 * f] = p.keys[0].x + 0.7 * f;
    data[3 * f + 1] = p.keys[0].y + 0.2 * f;
    data[3 * f + 2] = 1;
  }
  results.write(key, 0, 0, data);
}

const time = (label, fn, n = 1) => {
  fn();
  const t0 = performance.now();
  for (let i = 0; i < n; i++) fn();
  const ms = (performance.now() - t0) / n;
  console.log(`${label}: ${ms.toFixed(2)} ms`);
  return ms;
};

// Worst case: an edit that can affect any frame, then a draw at the end.
time("full integration (0..35999)", () => {
  project.touch(0);
  project.subjectState(s, N - 1, results);
}, 5);

// Warm cache: a timeline draw asks for ~1500 frames.
time("cached lookups (1500 frames)", () => {
  for (let i = 0; i < 1500; i++) project.subjectState(s, (i * N) / 1500 | 0, results);
}, 20);

// The interactive case: a key edit at the playhead, then a draw there.
time("drag at the playhead (key edit + draw)", () => {
  project.setKey(trackers[0].id, 30000, 1, 1);
  project.subjectState(s, 30000, results);
}, 60);

// The tracking case: the engine writes a 32-frame chunk just behind the playhead.
let w = 0;
time("results write behind the playhead + draw", () => {
  w += 32;
  results.write(keys[1], 0, 30000 + w, new Float32Array(96).fill(1));
  project.subjectState(s, 30032 + w, results);
}, 60);

// Scrubbing far ahead of the write region (the known worst case).
time("scrub to the end after a write at 1000", () => {
  results.write(keys[2], 0, 1000, new Float32Array(3));
  project.subjectState(s, N - 1, results);
}, 10);
