import assert from "node:assert/strict";
import { blendBox, combineTakes, compositeBounds, levelCoverage, passExtent } from "../static/js/bounds-stack.js";

// Composite bounds: takes average within a level (robustly), finer levels blend
// over coarser ones inside their coverage, and repeating a pass never shrinks
// the intended box. See editor/PLAN.md §3.

const near = (a, b, tol, msg) => assert.ok(Math.abs(a - b) <= tol, `${msg}: ${a} vs ${b} (±${tol})`);

// Identical takes: the box is not a confidence interval, so it must not shrink.
let c = combineTakes([{ box: [100, 100, 40, 30], weight: 1 }, { box: [100, 100, 40, 30], weight: 1 }]);
near(c.box[0], 100, 1e-9, "identical centers");
near(c.box[2], 40, 1e-9, "identical sizes stay");
near(c.box[3], 30, 1e-9, "identical sizes stay (h)");
assert.equal(c.uncertain, false);

// Disagreeing takes: averaged center, disagreement widens the region a little.
c = combineTakes([{ box: [90, 100, 40, 30], weight: 1 }, { box: [110, 100, 40, 30], weight: 1 }]);
near(c.box[0], 100, 1e-9, "centers average");
assert.ok(c.box[2] > 40, "disagreement adds a margin");
assert.ok(c.centerSpread > 5 && c.centerSpread < 8, `center spread ${c.centerSpread}`);

// Three or more takes: a wild take must not drag the center (weighted median).
c = combineTakes([
  { box: [100, 100, 40, 30], weight: 1 },
  { box: [100, 100, 40, 30], weight: 1 },
  { box: [1000, 100, 40, 30], weight: 1 },
]);
near(c.box[0], 100, 1e-9, "robust center ignores one wild take");
assert.equal(c.uncertain, true, "strong disagreement is marked uncertain");

// Composite: level 0 fallback, level 1 refinement with a 0.2 s endpoint fade.
const base = { id: 1, level: 0, kind: "base", enabled: true, weight: 1, a: 0, b: 99 };
const fine = { id: 2, level: 1, kind: "refine", enabled: true, weight: 1, a: 40, b: 60 };
const boxes = { 1: [100, 100, 50, 50], 2: [200, 100, 20, 20] };
const get = (p) => boxes[p.id];
const at = (f) => compositeBounds([base, fine], f, get, 10).box;
near(at(20)[0], 100, 1e-9, "outside the fine pass the coarse box remains");
near(at(20)[2], 50, 1e-9, "coarse size outside");
near(at(50)[0], 200, 1e-9, "inside the fine pass it wins");
near(at(50)[2], 20, 1e-9, "fine size inside");
near(at(40)[0], 150, 1e-9, "halfway through the 2-frame endpoint fade");
near(at(60)[0], 150, 1e-9, "fade at the other end");
assert.equal(compositeBounds([base, fine], 0, get, 10).takes, 1, "only the base covers frame 0");

// Weight scales a finer level's authority.
const half = { ...fine, weight: 0.5 };
near(compositeBounds([base, half], 50, get, 10).box[0], 150, 1e-9, "half-weight fine pass blends halfway");

// Disabling a level reveals the one below.
near(compositeBounds([base, { ...fine, enabled: false }], 50, get, 10).box[0], 100, 1e-9, "disabling the fine pass restores the coarse one");

// Missing coverage contributes nothing (no zero-sized box).
near(compositeBounds([base, { ...fine, a: 200, b: 260 }], 50, get, 10).box[0], 100, 1e-9, "fine pass without coverage at 50");

// Two takes at the same level average.
const take = { ...fine, id: 3, kind: "take" };
boxes[3] = [220, 100, 20, 20];
near(compositeBounds([base, fine, take], 50, get, 10).box[0], 210, 1e-9, "same-level takes average");

// Coverage helpers.
assert.deepEqual(passExtent([base, fine]), [0, 99]);
assert.deepEqual(levelCoverage([base, fine], 1), [40, 60]);
assert.deepEqual(levelCoverage([base, fine], 7), null);
near(blendBox([0, 0, 10, 10], [10, 10, 20, 20], 0.25)[0], 2.5, 1e-9, "blendBox");

console.log("Bounds stack: PASS");
