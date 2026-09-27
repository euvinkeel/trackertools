// Edit model: subjects own trackers; trackers own keyframes and manual ranges.
//
// A tracker (kind "point" = CoTracker point, "template" = template matcher)
// lives on frames [start, end). Each frame is either inside a manual range
// (position = linear interpolation of keyframes in that range) or in an auto
// region. Auto regions are split into segments at "seeds": the creation key,
// any key dropped in an auto region (a re-anchor), and the end of a manual
// range. Each segment is tracked independently from its seed, and its results
// are stored under a key derived from the seed, so editing a seed
// automatically invalidates only the affected segment.
//
// A subject's position stacks: the pushed position (see below) + offset (linear
// keys) + enabled finetune layers (keyed, additive). See editor/PLAN.md.

import { BOUNDS_CHANNELS, insideBounds } from "./bounds.js";
import { compositeBounds, passExtent } from "./bounds-stack.js";
import { ChunkStore, evalKeyed, f32ToB64 } from "./chunks.js";
import { applyAutoEnds, motionOutlierSet } from "./quality.js";
import { unpackMatch } from "./templates.js";

export const VIS_THRESHOLD = 0.6;
export const FORMAT_VERSION = 3;
const BASE_CHECKPOINT = 256; // frames between restorable integration states

const PALETTE = [
  "#22d3ee", "#f472b6", "#a3e635", "#fbbf24", "#818cf8",
  "#fb7185", "#34d399", "#f97316", "#c084fc", "#60a5fa",
];

export function trackerLabel(p) {
  return `${p.kind === "template" ? "T" : "P"}${p.id}`;
}

function subjectDefaults(s) {
  return {
    offset: [], bounds: null, boundsGuide: true, layers: [], activeLayer: null,
    boundsPasses: [], driftPolicy: "flag", escapeSeconds: 0.1, motionPolicy: "off", motionSeconds: 0.2,
    ...s,
  };
}

export class Project {
  constructor(frameCount, width = null, height = null, fps = 30) {
    this.frameCount = frameCount;
    this.width = width;
    this.height = height;
    this.fps = fps;
    this.subjects = [];
    this.trackers = [];
    this.stores = new Map(); // id -> ChunkStore (bounds, finetune keys)
    this.nextId = 1;
    this.version = 0;
    this.boundsRev = 0; // bumped whenever the effective bounds can change
    this._cache = null;
    this._base = new Map(); // subject id -> pushed-position integration (baseAt)
    this.edits = []; // recent touches: { v, from }, newest first (baseAt resumption)
  }

  // ---- persistence -----------------------------------------------------------------

  plain() {
    return { version: FORMAT_VERSION, subjects: this.subjects, trackers: this.trackers, nextId: this.nextId };
  }

  toJSON() {
    const stores = {};
    for (const [id, st] of this.stores) stores[id] = st.serialize();
    return { ...this.plain(), stores };
  }

  load(obj) {
    // v1 files call trackers "points" and have no kinds, offsets or stores.
    // v2 stored one bounds store per subject; it migrates to a base pass so
    // recorded passes can refine it later (see bounds-stack.js).
    const rawSubjects = structuredClone(obj.subjects || []);
    const hadPasses = rawSubjects.some((s) => s.boundsPasses !== undefined);
    this.subjects = rawSubjects.map(subjectDefaults);
    this.trackers = structuredClone(obj.trackers || obj.points || []).map((p) => ({ kind: "point", ...p }));
    this.stores = new Map(Object.entries(obj.stores || {}).map(([id, s]) => [id, ChunkStore.deserialize(s)]));
    this.nextId = obj.nextId || 1;
    for (const s of this.subjects) {
      if (s.bounds != null && !this.stores.has(String(s.bounds))) s.bounds = null;
      s.boundsPasses = (s.boundsPasses || []).filter((p) => p && this.stores.has(String(p.store)));
      if (!hadPasses) {
        const st = this.store(s.bounds);
        const ext = st && !st.empty ? st.extent() : null;
        if (ext) {
          s.boundsPasses = [{ id: this.nextId++, store: s.bounds, name: "Bounds", level: 0, kind: "base",
            enabled: true, weight: 1, a: ext[0], b: ext[1], samples: [], settings: {}, created: Date.now() }];
          s.bounds = null;
        }
      }
    }
    for (const p of this.trackers) {
      if (p.kind !== "template" || !p.looks) continue;
      p.retiredLooks = p.retiredLooks || [];
      let max = -1;
      p.looks.forEach((l, i) => {
        if (!Number.isInteger(l.slot)) l.slot = i;
        max = Math.max(max, l.slot);
      });
      p.nextLookSlot = Math.max(Number.isInteger(p.nextLookSlot) ? p.nextLookSlot : 0, max + 1);
    }
    this._base.clear();
    this.edits = [];
    this.touch();
  }

  // Undo snapshots: small structures as JSON, chunk stores by reference
  // (copy-on-write keeps them cheap).
  snapshot() {
    const stores = new Map();
    for (const [id, st] of this.stores) stores.set(id, st.snapshot());
    return { json: JSON.stringify(this.plain()), stores };
  }

  restore(snap) {
    const obj = JSON.parse(snap.json);
    this.subjects = obj.subjects.map(subjectDefaults);
    this.trackers = obj.trackers;
    this.nextId = obj.nextId;
    this.stores = new Map([...snap.stores].map(([id, s]) => [id, ChunkStore.fromSnapshot(s)]));
    this.touch();
  }

  sameAs(snap) {
    if (snap.stores.size !== this.stores.size) return false;
    for (const [id, st] of this.stores) {
      const s = snap.stores.get(id);
      if (!s || s.rev !== st.rev || s.chunks.size !== st.chunks.size) return false;
    }
    return JSON.stringify(this.plain()) === snap.json;
  }

  // Any model edit invalidates caches. `from` is the first frame the edit can
  // affect (0 when unknown), so the pushed-position cache can resume from a
  // checkpoint near it instead of integrating from the subject's first frame.
  touch(from = 0) {
    this.version++;
    this._cache = null;
    this.edits.unshift({ v: this.version, from });
    if (this.edits.length > 128) this.edits.pop();
  }

  // Earliest frame that may have changed since project version v. Infinity when
  // nothing did; 0 (assume anything) when the log no longer covers v.
  editsSince(v) {
    if (v === this.version) return Infinity;
    let from = Infinity;
    for (const e of this.edits) {
      if (e.v <= v) return from;
      if (e.from < from) from = e.from;
    }
    return 0;
  }

  get cache() {
    if (!this._cache) this._cache = { segs: new Map(), bySubject: null, index: null, subjects: null };
    return this._cache;
  }

  // ---- lookups ------------------------------------------------------------------

  subject(id) {
    const c = this.cache;
    if (!c.subjects) c.subjects = new Map(this.subjects.map((s) => [s.id, s]));
    return c.subjects.get(id) || null;
  }

  tracker(id) {
    return this.trackers.find((p) => p.id === id) || null;
  }

  trackersOf(subjectId) {
    const c = this.cache;
    if (!c.bySubject) {
      c.bySubject = new Map(this.subjects.map((s) => [s.id, []]));
      for (const p of this.trackers) c.bySubject.get(p.subjectId)?.push(p);
    }
    return c.bySubject.get(subjectId) || [];
  }

  store(id) {
    return id == null ? null : this.stores.get(String(id)) || null;
  }

  newStore(channels) {
    const id = String(this.nextId++);
    this.stores.set(id, new ChunkStore(channels));
    return id;
  }

  // Effective end (exclusive): the earliest of the user's end and an automatic
  // one. Keys, manual ranges and stored results beyond it are retained, so
  // clearing the end restores access to them (see §4).
  end(p) {
    let e = p.end ?? this.frameCount;
    if (p.autoEnd && p.autoEnd.f < e) e = p.autoEnd.f;
    return e;
  }

  // The user-defined end only: quality analysis must see past automatic ends.
  rawEnd(p) {
    return p.end ?? this.frameCount;
  }

  // Positions may lie outside the frame (manual animation only); trackers can
  // only start from inside it.
  isInside(x, y) {
    if (this.width == null) return true;
    return x >= 0 && y >= 0 && x <= this.width && y <= this.height;
  }

  // ---- subjects ---------------------------------------------------------------------

  addSubject(name) {
    const used = new Set(this.subjects.map((s) => s.color));
    const color = PALETTE.find((c) => !used.has(c)) || PALETTE[this.subjects.length % PALETTE.length];
    const s = subjectDefaults({ id: this.nextId++, name: name || `Subject ${this.subjects.length + 1}`, color, hidden: false });
    this.subjects.push(s);
    this.touch();
    return s;
  }

  removeSubject(id) {
    const s = this.subject(id);
    if (s) {
      if (s.bounds != null) this.stores.delete(String(s.bounds));
      for (const pass of s.boundsPasses || []) this.stores.delete(String(pass.store));
      for (const l of s.layers) this.stores.delete(String(l.keys));
    }
    this.subjects = this.subjects.filter((x) => x.id !== id);
    this.trackers = this.trackers.filter((p) => p.subjectId !== id);
    this._base.delete(id);
    this.touch();
  }

  updateSubject(id, patch) {
    Object.assign(this.subject(id), patch);
    this.touch();
  }

  // ---- offset (linear keys, held before the first and after the last) ---------------

  offsetAt(s, f) {
    const keys = s.offset;
    if (!keys.length) return { dx: 0, dy: 0 };
    if (f <= keys[0].f) return { dx: keys[0].dx, dy: keys[0].dy };
    const last = keys[keys.length - 1];
    if (f >= last.f) return { dx: last.dx, dy: last.dy };
    let lo = 0;
    let hi = keys.length - 1;
    while (hi - lo > 1) {
      const mid = (lo + hi) >> 1;
      if (keys[mid].f <= f) lo = mid;
      else hi = mid;
    }
    const a = keys[lo];
    const b = keys[hi];
    const t = (f - a.f) / (b.f - a.f);
    return { dx: a.dx + (b.dx - a.dx) * t, dy: a.dy + (b.dy - a.dy) * t };
  }

  offsetKeyAt(s, f) {
    return s.offset.find((k) => k.f === f) || null;
  }

  setOffsetKey(subjectId, f, dx, dy) {
    const s = this.subject(subjectId);
    if (!s) return;
    const i = s.offset.findIndex((k) => k.f >= f);
    const key = { f, dx, dy };
    if (i >= 0 && s.offset[i].f === f) s.offset[i] = key;
    else if (i < 0) s.offset.push(key);
    else s.offset.splice(i, 0, key);
    this.touch(f);
  }

  deleteOffsetKey(subjectId, f) {
    const s = this.subject(subjectId);
    if (!s) return;
    s.offset = s.offset.filter((k) => k.f !== f);
    this.touch(f);
  }

  clearOffset(subjectId) {
    const s = this.subject(subjectId);
    if (!s || !s.offset.length) return;
    const from = s.offset[0].f;
    s.offset = [];
    this.touch(from);
  }

  // ---- bounds (manual correction layer + recorded passes, see bounds-stack.js) ------

  boundsStore(s) {
    return s ? this.store(s.bounds) : null;
  }

  // The subject's manual bounds store, created on first use (call inside an edit).
  ensureBounds(subjectId) {
    const s = this.subject(subjectId);
    if (!s) return null;
    if (s.bounds == null || !this.store(s.bounds)) {
      s.bounds = this.newStore(BOUNDS_CHANNELS);
      this.touch();
    }
    return this.store(s.bounds);
  }

  // Effective bounds: the manual correction layer wins where it is defined;
  // otherwise the composite of the recorded passes.
  boundsAt(s, f) {
    const st = this.boundsStore(s);
    const manual = st ? st.get(f) : null;
    if (manual) return manual;
    return this.compositeAt(s, f)?.box ?? null;
  }

  // Composite of the recorded passes (cached per project version/frame).
  compositeAt(s, f) {
    const c = this.cache;
    if (!c.bounds) c.bounds = new Map();
    let entry = c.bounds.get(s.id);
    if (!entry) {
      entry = { memo: new Map() };
      c.bounds.set(s.id, entry);
    }
    if (entry.memo.has(f)) return entry.memo.get(f);
    const passes = s.boundsPasses || [];
    const out = passes.length
      ? compositeBounds(passes, f, (p, g) => {
          const st = this.store(p.store);
          return st ? st.get(g) : null;
        }, this.fps)
      : null;
    entry.memo.set(f, out);
    if (entry.memo.size > 8192) entry.memo.clear();
    return out;
  }

  // Frames covered by the manual layer or any enabled pass.
  boundsExtent(s) {
    let a = Infinity;
    let b = -Infinity;
    const ext = this.boundsStore(s)?.extent();
    if (ext) {
      a = ext[0];
      b = ext[1];
    }
    for (const p of s.boundsPasses || []) {
      if (p.enabled === false || p.a == null || p.b == null) continue;
      a = Math.min(a, p.a);
      b = Math.max(b, p.b);
    }
    return a <= b ? [a, b] : null;
  }

  boundsPass(subjectId, passId) {
    return (this.subject(subjectId)?.boundsPasses || []).find((p) => p.id === passId) || null;
  }

  // Bumped whenever the effective bounds can change (manual edits call this via
  // App.editBounds); automatic ends record it for provenance.
  boundsChanged() {
    this.boundsRev++;
  }

  // Record a pass (Puppeteer or imported). `boxes` is a Float32Array of
  // [cx, cy, w, h] per frame starting at `a`.
  addBoundsPass(subjectId, pass) {
    const s = this.subject(subjectId);
    if (!s) return null;
    this.boundsChanged();
    const store = this.newStore(BOUNDS_CHANNELS);
    const rec = {
      id: this.nextId++, store, name: pass.name || `Pass ${s.boundsPasses.length + 1}`,
      level: pass.level ?? 0, kind: pass.kind || "refine", enabled: pass.enabled !== false,
      weight: Number(pass.weight ?? 1), a: pass.a ?? null, b: pass.b ?? null,
      samples: pass.samples || [], settings: pass.settings || {}, created: Date.now(),
    };
    this._writePassBoxes(rec, pass.a, pass.boxes);
    s.boundsPasses.push(rec);
    this.touch(rec.a ?? 0);
    return rec;
  }

  _writePassBoxes(pass, a, boxes) {
    const st = this.store(pass.store);
    if (!st) return;
    st.clearRange(0, this.frameCount);
    if (!boxes) return;
    const n = boxes.length / 4;
    for (let i = 0; i < n; i++) st.set(a + i, Array.from(boxes.subarray(4 * i, 4 * i + 4)));
  }

  // Change a pass's settings/boxes; `regenerate` gets the record to rewrite.
  updateBoundsPass(subjectId, passId, patch, regenerate = null) {
    const pass = this.boundsPass(subjectId, passId);
    if (!pass) return null;
    this.boundsChanged();
    Object.assign(pass, patch);
    if (regenerate) regenerate(pass);
    this.touch(pass.a ?? 0);
    return pass;
  }

  removeBoundsPass(subjectId, passId) {
    const s = this.subject(subjectId);
    const pass = this.boundsPass(subjectId, passId);
    if (!s || !pass) return;
    this.boundsChanged();
    s.boundsPasses = s.boundsPasses.filter((p) => p !== pass);
    this.stores.delete(String(pass.store));
    this.touch(pass.a ?? 0);
  }

  // Replace a pass's synthesized boxes (settings or range changed; regenerate
  // from its original pointer samples).
  setBoundsPassBoxes(subjectId, passId, a, boxes) {
    const pass = this.boundsPass(subjectId, passId);
    if (!pass) return;
    this.boundsChanged();
    pass.a = a;
    this._writePassBoxes(pass, a, boxes);
    this.touch(a);
  }

  // "Clear bounds from here" applies to the recorded passes too: passes that
  // start at/after `from` are removed, ones that span it are trimmed.
  trimBoundsPasses(subjectId, from) {
    const s = this.subject(subjectId);
    if (!s) return null;
    this.boundsChanged();
    let a = Infinity;
    for (const p of [...s.boundsPasses]) {
      if (p.a == null || p.b == null) continue;
      if (p.a >= from) {
        s.boundsPasses = s.boundsPasses.filter((q) => q !== p);
        this.stores.delete(String(p.store));
        a = Math.min(a, p.a);
      } else if (p.b >= from) {
        p.b = from - 1;
        this.store(p.store)?.clearRange(from, this.frameCount);
        a = Math.min(a, from);
      }
    }
    return Number.isFinite(a) ? a : null;
  }

  // Recompute automatic ends from stored observations (derived state, not an
  // undo step). Returns the trackers whose end changed.
  recomputeAutoEnds(subjectId, results) {
    const s = this.subject(subjectId);
    if (!s) return [];
    const changed = applyAutoEnds(this, results, s, { fps: this.fps });
    if (changed.length) {
      // An added or cleared end changes which frames contribute, so the
      // pushed-position cache must not resume past the tracker's start.
      let from = Infinity;
      for (const p of changed) from = Math.min(from, Math.max(0, p.start));
      this.touch(Number.isFinite(from) ? from : 0);
    }
    return changed;
  }

  // A tracked position outside its subject's bounds (+ margin) has drifted.
  isDrifted(p, f, x, y) {
    const b = this.boundsAt(this.subject(p.subjectId), f);
    return !!b && !insideBounds(b, x, y);
  }

  // Confirmed motion-consistency outlier (only when the subject asks for
  // exclusion). See motion.js / quality.js.
  isMotionOutlier(p, f, results) {
    const s = this.subject(p.subjectId);
    if (!s || (s.motionPolicy ?? "off") !== "exclude") return false;
    return motionOutlierSet(this, results, s, f, { fps: this.fps }).has(p.id);
  }

  // First frame of the drifted run that contains f (f itself if not drifted before).
  driftedRunStart(p, f, results) {
    let g = f;
    while (g - 1 >= p.start && this.trackerState(p, g - 1, results)?.drifted) g--;
    return g;
  }

  // ---- finetune layers (additive keyed corrections) ------------------------------------

  layerSum(s, f) {
    let dx = 0;
    let dy = 0;
    for (const l of s.layers) {
      if (!l.enabled || !l.weight) continue;
      const st = this.store(l.keys);
      const v = st && evalKeyed(st, f);
      if (v) {
        dx += v[0] * l.weight;
        dy += v[1] * l.weight;
      }
    }
    return { dx, dy };
  }

  // ---- trackers -------------------------------------------------------------------------

  addPoint(subjectId, f, x, y, manual = false) {
    const p = {
      id: this.nextId++, subjectId, kind: "point", start: f, end: null, keys: [{ f, x, y }],
      manual: manual ? [{ a: f, b: null }] : [],
    };
    this.trackers.push(p);
    this.touch(f);
    return p;
  }

  addTemplate(subjectId, look, threshold = 0.7) {
    const p = {
      id: this.nextId++, subjectId, kind: "template", start: look.f, end: null,
      keys: [{ f: look.f, x: look.x + look.hx, y: look.y + look.hy }],
      manual: [], threshold, looks: [{ ...look, id: this.nextId++, rev: 1, slot: 0 }],
      retiredLooks: [], nextLookSlot: 1,
    };
    this.trackers.push(p);
    this.touch(look.f);
    return p;
  }

  // Looks get stable, never-reused slots: packed results identify a look by
  // slot, so removing a look must not shift the meaning of preserved results.
  // Removed looks move to retiredLooks so historical results still resolve.
  addLook(id, look) {
    const p = this.tracker(id);
    if (!p || p.kind !== "template") return null;
    const rec = { ...look, id: this.nextId++, rev: 1, slot: p.nextLookSlot ?? p.looks.length };
    p.nextLookSlot = rec.slot + 1;
    p.looks.push(rec);
    this.touch(p.start);
    return rec;
  }

  updateLook(id, lookId, patch) {
    const p = this.tracker(id);
    const look = p?.looks?.find((l) => l.id === lookId);
    if (!look) return;
    Object.assign(look, patch);
    look.rev++;
    this.touch(p.start);
  }

  removeLook(id, lookId) {
    const p = this.tracker(id);
    if (!p || p.kind !== "template" || p.looks.length <= 1) return;
    const i = p.looks.findIndex((l) => l.id === lookId);
    if (i < 0) return;
    p.retiredLooks = p.retiredLooks || [];
    p.retiredLooks.push(p.looks[i]);
    p.looks.splice(i, 1);
    this.touch(p.start);
  }

  lookBySlot(p, slot) {
    return p.looks?.find((l) => l.slot === slot) || p.retiredLooks?.find((l) => l.slot === slot) || null;
  }

  lookIndex(p, slot) {
    return p.looks?.findIndex((l) => l.slot === slot) ?? -1;
  }

  setThreshold(id, threshold) {
    const p = this.tracker(id);
    if (!p || p.kind !== "template") return;
    p.threshold = Math.max(-1, Math.min(1, threshold));
    this.touch(p.start);
  }

  deleteTracker(id) {
    this.trackers = this.trackers.filter((p) => p.id !== id);
    this.touch();
  }

  // "Remove" from frame f onward. On the first frame this deletes the tracker
  // (an explicit gesture). Later keys and manual ranges are *retained* so
  // clearing the end restores access to them (see §4).
  endTrackerAt(id, f) {
    const p = this.tracker(id);
    if (!p) return;
    if (f <= p.start) {
      this.deleteTracker(id);
      return;
    }
    const from = Math.min(this.end(p), f);
    p.end = f;
    if (p.autoEnd) {
      // An explicit end beyond an automatic one overrides it; either way the
      // automatic end must not silently come back.
      if (f > p.autoEnd.f) delete p.autoEnd;
      p.noAutoEnd = true;
    }
    this.touch(from);
  }

  // An automatic end (bounds/motion policy). Never deletes: even an end at the
  // creation frame keeps the tracker, so the user can review and clear it.
  autoEndTrackerAt(id, f, reason = "bounds", confirmedAt = f) {
    const p = this.tracker(id);
    if (!p) return;
    const from = Math.min(this.end(p), f);
    p.autoEnd = { f, reason, confirmedAt };
    delete p.noAutoEnd;
    this.touch(from);
  }

  // Remove every end (user or automatic). Removing an automatic one also turns
  // automatic ending off for this tracker until explicitly re-enabled, or it
  // would immediately reappear on the next analysis.
  clearEnd(id) {
    const p = this.tracker(id);
    if (!p) return;
    if (p.autoEnd) p.noAutoEnd = true;
    p.end = null;
    delete p.autoEnd;
    this.touch(p.start);
  }

  // Re-enable automatic ending for a tracker the user overrode.
  setAutoEnd(id, on) {
    const p = this.tracker(id);
    if (!p) return;
    if (on) delete p.noAutoEnd;
    else {
      p.noAutoEnd = true;
      delete p.autoEnd;
    }
    this.touch(p.start);
  }

  // Move the end marker (timeline drag). Moving an automatic end converts it
  // into a user boundary with the same override behavior.
  setTrackerEnd(id, f) {
    const p = this.tracker(id);
    if (!p) return;
    if (f <= p.start) return;
    const from = Math.min(this.end(p), f);
    if (p.autoEnd) {
      delete p.autoEnd;
      p.noAutoEnd = true;
    }
    p.end = f >= this.frameCount ? null : f;
    this.touch(from);
  }

  // Dragging a tracker: a keyframe inside manual ranges, a re-anchor elsewhere.
  setKey(id, f, x, y) {
    const p = this.tracker(id);
    if (!p || f < p.start || f >= this.end(p)) return false;
    const i = p.keys.findIndex((k) => k.f >= f);
    if (i >= 0 && p.keys[i].f === f) p.keys[i] = { f, x, y };
    else if (i < 0) p.keys.push({ f, x, y });
    else p.keys.splice(i, 0, { f, x, y });
    this.touch(f);
    return true;
  }

  deleteKey(id, f) {
    const p = this.tracker(id);
    if (!p || f === p.start) return false;
    const n = p.keys.length;
    p.keys = p.keys.filter((k) => k.f !== f);
    if (p.keys.length === n) return false;
    // Removing a key changes the interpolation from the key before it on.
    let prev = p.start;
    for (const k of p.keys) if (k.f < f) prev = k.f;
    this.touch(prev);
    return true;
  }

  // Toggle manual animation at frame f. Turning it on starts a manual range
  // at f keyed with the current position; turning it off ends the range at f
  // so auto tracking resumes from the manual position there.
  toggleManualAt(id, f, pos) {
    const p = this.tracker(id);
    if (!p || f < p.start || f >= this.end(p)) return null;
    const r = this.manualRangeAt(p, f);
    if (r) {
      const rEnd = r.b ?? this.end(p);
      if (f === r.a) {
        p.manual = p.manual.filter((m) => m !== r);
        p.keys = p.keys.filter((k) => k.f === p.start || k.f < r.a || k.f >= rEnd);
      } else {
        r.b = f;
        p.keys = p.keys.filter((k) => !(k.f >= f && k.f < rEnd));
      }
      this.touch(f);
      return "auto";
    }
    const next = p.manual.find((m) => m.a > f);
    const range = { a: f, b: next ? next.b : null };
    if (next) p.manual = p.manual.filter((m) => m !== next);
    p.manual.push(range);
    p.manual.sort((m1, m2) => m1.a - m2.a);
    if (pos) this.setKey(id, f, pos.x, pos.y);
    this.touch(f);
    return "manual";
  }

  // ---- derived state -----------------------------------------------------------------

  manualRangeAt(p, f, end = this.end(p)) {
    for (const r of p.manual) {
      if (f >= r.a && f < (r.b ?? end)) return r;
    }
    return null;
  }

  manualPos(p, r, f, end = this.end(p)) {
    const rEnd = r.b ?? end;
    let prev = null;
    let next = null;
    for (const k of p.keys) {
      if (k.f < r.a) continue;
      if (k.f >= rEnd) break;
      if (k.f <= f) prev = k;
      else {
        next = k;
        break;
      }
    }
    if (prev && next) {
      const t = (f - prev.f) / (next.f - prev.f);
      return { x: prev.x + (next.x - prev.x) * t, y: prev.y + (next.y - prev.y) * t };
    }
    const k = prev || next;
    return k ? { x: k.x, y: k.y } : null;
  }

  // A segment's results key. Deliberately independent of the look set: adding,
  // editing or removing a look only affects tracking from the cursor on (the
  // frames before it were tracked with the previous look set and stay valid),
  // so re-keying on a look change would throw all of that away. See §7.
  segmentKey(p, s) {
    return `${p.id}@${s.f}@${s.x.toFixed(2)},${s.y.toFixed(2)}`;
  }

  // Segments for a lifespan (defaults to the effective end). Quality analysis
  // asks for the raw end so automatic ends don't hide their own evidence.
  segments(p, end = this.end(p)) {
    const c = this.cache;
    const cacheKey = `${p.id}:${end}`;
    let segs = c.segs.get(cacheKey);
    if (segs) return segs;
    segs = [];
    const regions = [];
    let cursor = p.start;
    let after = null;
    for (const r of p.manual) {
      if (r.a >= end) break;
      if (cursor < r.a) regions.push({ a: cursor, b: r.a, after });
      cursor = Math.max(cursor, r.b ?? end);
      after = r;
    }
    if (cursor < end) regions.push({ a: cursor, b: end, after });

    for (const reg of regions) {
      const keysIn = p.keys.filter((k) => k.f >= reg.a && k.f < reg.b);
      const seeds = [];
      if (!keysIn.length || keysIn[0].f !== reg.a) {
        const pos = reg.after ? this.manualPos(p, reg.after, reg.a) : null;
        if (pos) seeds.push({ f: reg.a, x: pos.x, y: pos.y });
      }
      seeds.push(...keysIn);
      seeds.forEach((s, i) => {
        segs.push({
          key: this.segmentKey(p, s),
          trackerId: p.id,
          subjectId: p.subjectId,
          kind: p.kind,
          q: s.f,
          x: s.x,
          y: s.y,
          end: i + 1 < seeds.length ? seeds[i + 1].f : reg.b,
          offframe: !this.isInside(s.x, s.y),
        });
      });
    }
    c.segs.set(cacheKey, segs);
    return segs;
  }

  segmentAt(p, f, end = this.end(p)) {
    const segs = this.segments(p, end);
    let lo = 0;
    let hi = segs.length - 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      const s = segs[mid];
      if (f < s.q) hi = mid - 1;
      else if (f >= s.end) lo = mid + 1;
      else return s;
    }
    return null;
  }

  isKey(p, f) {
    return p.keys.some((k) => k.f === f);
  }

  // State of a tracker on frame f:
  //   null                 tracker doesn't exist on this frame
  //   {mode: "manual"}     manually animated
  //   {mode: "auto"}       tracked (vis = visibility 0..1)
  //   {mode: "pending"}    needs tracking; x/y = last known position
  //   {mode: "blocked"}    auto segment seeded off-frame; can't be tracked
  //   {mode: "nodata"}     no seed covers this frame
  // `end` defaults to the effective lifespan; quality analysis passes the raw
  // end so automatic ends don't hide the observations they are based on.
  trackerState(p, f, results, end = this.end(p)) {
    if (f < p.start || f >= end) return null;
    const r = this.manualRangeAt(p, f, end);
    if (r) {
      const pos = this.manualPos(p, r, f, end);
      return pos ? { mode: "manual", x: pos.x, y: pos.y, vis: 1, range: r } : { mode: "nodata" };
    }
    const s = this.segmentAt(p, f, end);
    if (!s) return { mode: "nodata" };
    if (s.offframe) return { mode: "blocked", x: s.x, y: s.y, seg: s };
    if (f === s.q) return { mode: "auto", x: s.x, y: s.y, vis: 1, seg: s, anchor: true,
      drifted: this.isDrifted(p, f, s.x, s.y),
      ...(p.kind === "template" ? { score: 1, look: p.looks[0]?.slot ?? 0, lookIndex: 0, lost: false } : {}) };
    const v = results.get(s.key, f);
    if (v) {
      if (p.kind === "template") {
        const { slot, score } = unpackMatch(v[2]);
        const lost = score < p.threshold;
        return { mode: "auto", x: v[0], y: v[1], vis: lost ? 0 : 1, score, look: slot,
          lookIndex: this.lookIndex(p, slot), lost,
          drifted: !lost && this.isDrifted(p, f, v[0], v[1]), seg: s };
      }
      return { mode: "auto", x: v[0], y: v[1], vis: v[2], drifted: this.isDrifted(p, f, v[0], v[1]), seg: s };
    }
    let gx = s.x;
    let gy = s.y;
    const hi = results.hi(s.key);
    if (hi > s.q) {
      const g = results.get(s.key, Math.min(hi, f));
      if (g) {
        gx = g[0];
        gy = g[1];
      }
    }
    return { mode: "pending", x: gx, y: gy, seg: s };
  }

  // Whether a tracker state counts toward its subject's position.
  contributes(st) {
    return !!st && (st.mode === "manual" || st.mode === "auto") && !st.drifted && !st.lost;
  }

  // As contributes(), minus confirmed motion outliers when the subject asks for
  // exclusion (see motion.js / quality.js).
  contributesAt(p, f, st, results) {
    return this.contributes(st) && !this.isMotionOutlier(p, f, results);
  }

  // ---- the pushed position ---------------------------------------------------------------
  //
  // A subject's center is *not* the mean of its trackers' absolute positions.
  // It is first defined by the mean of the trackers that contribute on its
  // first frame, and from then on it is pushed, frame by frame, by the mean of
  // those trackers' motions: each tracker's delta since the last frame it
  // contributed. Adding or removing trackers therefore never makes the subject
  // jump to a new absolute mean, and a manual adjustment is just an offset key
  // that re-anchors where the pushing continues from. A frame where nothing
  // contributes holds the position. The integration is cached per model +
  // results version and computed up to the frame asked for; edits record the
  // first frame they can affect, and the cache resumes from a checkpoint just
  // before it instead of integrating from the start. See PLAN.md §2.

  _baseEntry(s, results) {
    let e = this._base.get(s.id);
    const trackers = this.trackersOf(s.id);
    if (e && e.uid === results.uid && e.pv === this.version && e.rv === results.version) return e;
    if (!e) {
      e = {
        uid: results.uid, pv: 0, rv: -1, results, trackers: [],
        from: Infinity, to: -Infinity,
        xs: new Float32Array(this.frameCount).fill(NaN),
        ys: new Float32Array(this.frameCount).fill(NaN),
        prevX: null, prevY: null, bx: NaN, by: NaN,
        cps: new Map(), // frame -> integration state after that frame
      };
      this._base.set(s.id, e);
      this._resetBase(e, trackers, 0);
    } else {
      const same = e.trackers.length === trackers.length && trackers.every((p, i) => p === e.trackers[i]);
      // Resume from the newest checkpoint before the earliest change. A change
      // to the tracker set invalidates everything.
      const d = same ? Math.min(this.editsSince(e.pv), results.uid === e.uid ? results.editsSince(e.rv) : 0) : 0;
      if (d !== Infinity) this._resetBase(e, trackers, d);
    }
    e.uid = results.uid;
    e.results = results;
    e.pv = this.version;
    e.rv = results.version;
    return e;
  }

  _resetBase(e, trackers, d) {
    e.trackers = trackers;
    e.from = Infinity;
    for (const p of trackers) e.from = Math.min(e.from, Math.max(0, p.start));
    let best = -1;
    let cp = null;
    for (const [f, c] of e.cps) {
      if (f < d && f > best) {
        best = f;
        cp = c;
      }
    }
    if (cp) {
      e.to = best;
      e.bx = cp.bx;
      e.by = cp.by;
      e.prevX = cp.px.slice();
      e.prevY = cp.py.slice();
      e.bad = cp.bad ? cp.bad.slice() : new Uint8Array(e.trackers.length);
    } else {
      e.to = e.from - 1;
      e.bx = NaN;
      e.by = NaN;
      e.prevX = null;
      e.prevY = null;
      e.bad = new Uint8Array(e.trackers.length);
    }
    for (const f of [...e.cps.keys()]) if (f >= d) e.cps.delete(f);
  }

  _baseExtend(e, upto) {
    const f1 = Math.min(upto, this.frameCount - 1);
    const start = Math.max(e.from, e.to + 1);
    if (!e.trackers.length || start > f1) return;
    if (e.prevX == null) {
      e.prevX = new Float64Array(e.trackers.length).fill(NaN);
      e.prevY = new Float64Array(e.trackers.length).fill(NaN);
    }
    if (!e.bad) e.bad = new Uint8Array(e.trackers.length);
    for (let f = start; f <= f1; f++) {
      let dx = 0, dy = 0, np = 0, ax = 0, ay = 0, na = 0, nNew = 0;
      for (let i = 0; i < e.trackers.length; i++) {
        const p = e.trackers[i];
        const st = this.trackerState(p, f, e.results);
        // A confirmed failure (drifted outside the bounds or not found) breaks
        // this tracker's motion reference: when it comes back, its accumulated
        // rejected displacement must not be injected into the subject.
        if (st && st.mode === "auto" && (st.drifted || st.lost)) {
          e.bad[i] = 1;
          continue;
        }
        if (!this.contributes(st)) continue;
        // A confirmed motion outlier is rejected like a drift failure: its
        // reference resets, so the rejected displacement is never injected.
        if (this.isMotionOutlier(p, f, e.results)) {
          e.bad[i] = 1;
          continue;
        }
        na++;
        ax += st.x;
        ay += st.y;
        if (Number.isNaN(e.prevX[i])) nNew++;
        if (!Number.isNaN(e.prevX[i]) && !e.bad[i]) {
          dx += st.x - e.prevX[i];
          dy += st.y - e.prevY[i];
          np++;
        }
        e.bad[i] = 0;
        e.prevX[i] = st.x;
        e.prevY[i] = st.y;
      }
      if (np) {
        e.bx += dx / np;
        e.by += dy / np;
      } else if (na && (Number.isNaN(e.bx) || nNew === na)) {
        // Nothing has moved relative to anything yet (the first frames, or every
        // contributing tracker is new): anchor to the mean of what contributes.
        // Trackers whose reference was reset by a failure hold instead — the
        // subject keeps the position the surviving trackers pushed it to.
        e.bx = ax / na;
        e.by = ay / na;
      }
      // Always write: a reset must overwrite every value, including with NaN
      // where the base is undefined (e.g. everything drifted).
      e.xs[f] = Number.isFinite(e.bx) ? e.bx : NaN;
      e.ys[f] = Number.isFinite(e.by) ? e.by : NaN;
      if ((f & (BASE_CHECKPOINT - 1)) === BASE_CHECKPOINT - 1) {
        e.cps.set(f, { bx: e.bx, by: e.by, px: e.prevX.slice(), py: e.prevY.slice(), bad: e.bad.slice() });
      }
    }
    e.to = f1;
  }

  // The pushed position on frame f as [x, y], or null where it is undefined.
  baseAt(s, f, results) {
    const e = this._baseEntry(s, results);
    if (f > e.to) this._baseExtend(e, f);
    return Number.isNaN(e.xs[f]) ? null : [e.xs[f], e.ys[f]];
  }

  // Subject position on frame f: the pushed position (hidden CoTracker points
  // included), plus offset and finetune layers.
  subjectState(s, f, results) {
    let n = 0, visible = 0, hidden = 0, pending = 0, lost = 0, drifted = 0, outliers = 0, exists = 0;
    const watchMotion = (s.motionPolicy ?? "off") !== "off";
    for (const p of this.trackersOf(s.id)) {
      const st = this.trackerState(p, f, results);
      if (!st) continue;
      exists++;
      const excluded = watchMotion && this.isMotionOutlier(p, f, results);
      if (excluded) outliers++;
      if (this.contributes(st) && !excluded) {
        n++;
        if (st.vis >= VIS_THRESHOLD) visible++;
        else hidden++;
      } else if (st.lost) lost++;
      else if (st.drifted) drifted++;
      else if (!excluded) pending++;
    }
    const base = { n, visible, hidden, pending, lost, drifted, outliers };
    if (!exists) return { status: "none", ...base };
    if (!n) return { status: pending ? "unknown" : "lost", ...base };
    const raw = this.baseAt(s, f, results);
    if (!raw) return { status: "unknown", ...base };
    const [rawX, rawY] = raw;
    const off = this.offsetAt(s, f);
    const fine = this.layerSum(s, f);
    return {
      status: pending ? "partial" : "ok",
      ...base,
      x: rawX + off.dx + fine.dx,
      y: rawY + off.dy + fine.dy,
      rawX,
      rawY,
      offX: off.dx,
      offY: off.dy,
      fineX: fine.dx,
      fineY: fine.dy,
    };
  }

  segmentIndex() {
    const c = this.cache;
    if (!c.index) {
      c.index = new Map();
      for (const p of this.trackers) for (const s of this.segments(p)) c.index.set(s.key, s);
    }
    return c.index;
  }

  // Segments the tracker needs to process for a run starting at startFrame,
  // optionally limited to a set of tracker ids. Partially tracked segments
  // resume from their last confidently visible on-frame position so the model
  // re-samples the appearance from a clean view. Segments seeded off-frame
  // can't be tracked and are counted in `blocked`.
  planRun(startFrame, results, onlyTrackerIds = null) {
    const segments = [];
    let blocked = 0;
    for (const p of this.trackers) {
      if (onlyTrackerIds && !onlyTrackerIds.has(p.id)) continue;
      for (const s of this.segments(p)) {
        if (s.end <= startFrame) continue;
        const hi = results.hi(s.key);
        if (hi >= s.end - 1) continue;
        if (s.offframe) {
          blocked++;
          continue;
        }
        let q = s.q, x = s.x, y = s.y;
        for (let f = Math.min(hi, s.end - 1); f > s.q && f > hi - 3000; f--) {
          const v = results.get(s.key, f);
          const good = p.kind === "template" ? v && unpackMatch(v[2]).score >= p.threshold : v && v[2] >= VIS_THRESHOLD;
          if (good && this.isInside(v[0], v[1]) && !this.isDrifted(p, f, v[0], v[1])) {
            q = f;
            x = v[0];
            y = v[1];
            break;
          }
        }
        segments.push({ key: s.key, trackerId: p.id, q, x, y, end: s.end, kind: p.kind, subjectId: p.subjectId,
          ...(p.kind === "template" ? { threshold: p.threshold, looks: p.looks } : {}) });
      }
    }
    return { segments, blocked, bounds: this.runBounds(segments) };
  }

  // Bounds of the subjects in a run, from their earliest segment on:
  // { [subjectId]: { f0, data: base64 Float32 cx,cy,w,h per frame, guide } }.
  runBounds(segments) {
    const first = new Map();
    for (const seg of segments) first.set(seg.subjectId, Math.min(first.get(seg.subjectId) ?? Infinity, seg.q));
    const out = {};
    for (const [sid, q] of first) {
      const s = this.subject(sid);
      const ext = this.boundsExtent(s);
      if (!ext) continue;
      const f0 = Math.max(q, ext[0]);
      if (ext[1] < f0) continue;
      out[sid] = { f0, data: f32ToB64(this.packBoundsRange(s, f0, ext[1])), guide: s.boundsGuide !== false };
    }
    return out;
  }

  // Effective bounds (manual layer or pass composite) over [a, b] as a
  // Float32Array with NaN where undefined.
  packBoundsRange(s, a, b) {
    const arr = new Float32Array((b - a + 1) * 4).fill(NaN);
    for (let f = a; f <= b; f++) {
      const v = this.boundsAt(s, f);
      if (v) arr.set(v, (f - a) * 4);
    }
    return arr;
  }

  // Result keys belonging to trackers that still exist, including dormant
  // results past a tracker's end (save must not drop them).
  resultKeys(results) {
    const ids = new Set(this.trackers.map((p) => String(p.id)));
    const out = [];
    for (const key of results.tracks.keys()) {
      const at = key.indexOf("@");
      if (at > 0 && ids.has(key.slice(0, at))) out.push(key);
    }
    return out;
  }
}
