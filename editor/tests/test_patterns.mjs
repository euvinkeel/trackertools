import assert from "node:assert/strict";
import {
  clearPatterns, deletePattern, exportPatterns, importPatterns, isPersistent,
  listPatterns, patternById, patternBytes, renamePattern, savePattern,
} from "../static/js/patterns.js";

// Pattern library storage: CRUD, name replacement, limits, export/import and
// the memory fallback. Node has no localStorage, so this exercises the shim.
clearPatterns();
assert.equal(isPersistent(), false, "Node falls back to the in-memory store (labeled temporary)");
assert.equal(listPatterns().length, 0);

const a = savePattern({ name: "Cursor", w: 20, h: 26, hx: 2.5, hy: 2.5, tmpl: "AAAA", mask: "" });
assert.equal(listPatterns().length, 1);
assert.equal(patternById(a.id).name, "Cursor");
assert.equal(patternById(a.id).w, 20);

// Saving under the same name (any case) replaces in place and keeps the id.
const b = savePattern({ name: "cursor", w: 10, h: 12, hx: 1, hy: 1, tmpl: "BBBB", mask: "x" });
assert.equal(b.id, a.id);
assert.equal(listPatterns().length, 1);
assert.equal(patternById(a.id).w, 10);
assert.equal(patternById(a.id).mask, "x");

// An explicit id updates that entry (the editor saving over its pattern).
const c = savePattern({ id: a.id, name: "Mouse", w: 8, h: 8, hx: 0, hy: 0, tmpl: "CCCC", mask: "" });
assert.equal(c.id, a.id);
assert.equal(patternById(a.id).name, "Mouse");

// Invalid entries are dropped on read.
assert.equal(listPatterns().length, 1);

renamePattern(a.id, "Pointer");
assert.equal(patternById(a.id).name, "Pointer");
assert.throws(() => renamePattern("missing", "x"), /no longer exists/);
assert.throws(() => savePattern({ name: "  ", tmpl: "AAAA" }), /name/);
assert.throws(() => savePattern({ name: "Empty", tmpl: "" }), /pixels/);

// Export/import round trip: names dedupe, ids are regenerated.
savePattern({ name: "Second", w: 12, h: 12, hx: 2, hy: 2, tmpl: "DDDD", mask: "" });
const text = exportPatterns();
assert.match(text, /cotrack\.patterns/);
clearPatterns();
assert.equal(listPatterns().length, 0);
let res = importPatterns(text);
assert.equal(res.added, 2);
assert.equal(res.total, 2);
assert.deepEqual(listPatterns().map((p) => p.name).sort(), ["Pointer", "Second"]);
// Importing the same library again replaces by name instead of duplicating.
res = importPatterns(text);
assert.equal(res.total, 2);
// Replacing the whole library.
res = importPatterns(exportPatterns(), { replace: true });
assert.equal(res.total, 2);
assert.throws(() => importPatterns("not json"), /valid JSON/);
assert.throws(() => importPatterns('{"a":1}'), /pattern library/);

// The cap is enforced without silently dropping the oldest entries.
const big = "A".repeat(4_100_000);
assert.throws(() => savePattern({ name: "Huge", w: 20, h: 20, hx: 1, hy: 1, tmpl: big, mask: "" }), /full/);
assert.equal(patternById("nope"), null);
deletePattern(listPatterns()[0].id);
assert.equal(listPatterns().length, 1);
assert.ok(patternBytes() > 0);

console.log("Pattern library: PASS");
