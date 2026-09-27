// Stores auto-tracking output per segment key: x, y, visibility per frame.
// Chunked Float32Arrays keep memory small and lookups O(1) for long videos.

const CHUNK_BITS = 10;
const CHUNK = 1 << CHUNK_BITS;
const MASK = CHUNK - 1;
let uid = 0;

export class ResultStore {
  constructor() {
    this.tracks = new Map();
    this.version = 0;
    this.uid = ++uid; // identity, so caches can key on the store
    this.edits = []; // recent writes: { v, from }, newest first (cache resumption)
    // key -> first frame that may still be written. A look/bounds correction at
    // frame f truncates the results from f on and freezes the prefix: a run that
    // warms up from a frame before f must not overwrite the history the
    // correction promised to preserve.
    this.boundaries = new Map();
  }

  // Note a change: version bump + the earliest frame it can affect.
  _note(from) {
    this.version++;
    this.edits.unshift({ v: this.version, from });
    if (this.edits.length > 128) this.edits.pop();
  }

  // Earliest frame that may have changed since results version v (Infinity when
  // nothing did; 0 when the log no longer covers v).
  editsSince(v) {
    if (v === this.version) return Infinity;
    let from = Infinity;
    for (const e of this.edits) {
      if (e.v <= v) return from;
      if (e.from < from) from = e.from;
    }
    return 0;
  }

  _track(key, q) {
    let t = this.tracks.get(key);
    if (!t) {
      t = { q, hi: q - 1, chunks: new Map() };
      this.tracks.set(key, t);
    }
    return t;
  }

  write(key, q, f0, data) {
    const b = this.boundaries.get(key);
    if (b != null) {
      const n0 = data.length / 3;
      if (f0 + n0 <= b) return;
      if (f0 < b) {
        const skip = b - f0;
        data = ArrayBuffer.isView(data) ? data.subarray(skip * 3) : data.slice(skip * 3);
        f0 = b;
      }
    }
    const t = this._track(key, q);
    const n = data.length / 3;
    for (let i = 0; i < n; i++) {
      const f = f0 + i;
      const ci = f >> CHUNK_BITS;
      let arr = t.chunks.get(ci);
      if (!arr) {
        arr = new Float32Array(CHUNK * 3).fill(NaN);
        t.chunks.set(ci, arr);
      }
      const o = (f & MASK) * 3;
      arr[o] = data[3 * i];
      arr[o + 1] = data[3 * i + 1];
      arr[o + 2] = data[3 * i + 2];
    }
    if (f0 <= t.hi + 1) {
      let hi = Math.max(t.hi, f0 + n - 1);
      while (this._has(t, hi + 1)) hi++;
      t.hi = hi;
    }
    this._note(f0);
  }

  _has(t, f) {
    const arr = t.chunks.get(f >> CHUNK_BITS);
    return !!arr && !Number.isNaN(arr[(f & MASK) * 3]);
  }

  // Returns [x, y, visibility] or null.
  get(key, f) {
    const t = this.tracks.get(key);
    if (!t) return null;
    const arr = t.chunks.get(f >> CHUNK_BITS);
    if (!arr) return null;
    const o = (f & MASK) * 3;
    const x = arr[o];
    if (Number.isNaN(x)) return null;
    return [x, arr[o + 1], arr[o + 2]];
  }

  // Last frame of the contiguous computed run starting at the seed frame.
  hi(key) {
    const t = this.tracks.get(key);
    return t ? t.hi : -Infinity;
  }

  // Forget computed frames from f onward so a subsequent Go recomputes them.
  // Returns a patch for restore() (the removed chunks, never mutated), or null
  // when nothing was removed. When frames were removed, the prefix is frozen:
  // later runs may not write before f for this key (see boundaries above).
  truncate(key, f) {
    const t = this.tracks.get(key);
    if (!t) return null;
    const patch = { key, q: t.q, hi: t.hi, chunks: [] };
    for (const [ci, arr] of [...t.chunks]) {
      const start = ci * CHUNK;
      if (start + CHUNK <= f) continue;
      patch.chunks.push([ci, arr]);
      if (start >= f) t.chunks.delete(ci);
      else {
        const copy = arr.slice();
        copy.fill(NaN, (f - start) * 3);
        t.chunks.set(ci, copy);
      }
    }
    if (!patch.chunks.length) return null;
    t.hi = Math.min(t.hi, f - 1);
    patch.boundary = this.boundaries.has(key) ? this.boundaries.get(key) : null;
    this.boundaries.set(key, f);
    this._note(f);
    return patch;
  }

  // Undo truncations (patches from truncate, applied newest first).
  restore(patches) {
    let from = Infinity;
    for (const p of [...patches].reverse()) {
      const t = this._track(p.key, p.q);
      for (const [ci, arr] of p.chunks) {
        t.chunks.set(ci, arr);
        if (ci * CHUNK < from) from = ci * CHUNK;
      }
      t.hi = p.hi;
      if (p.boundary == null) this.boundaries.delete(p.key);
      else this.boundaries.set(p.key, p.boundary);
    }
    if (patches.length) this._note(from);
  }

  // Frames before this key's boundary are frozen (see truncate).
  boundary(key) {
    return this.boundaries.has(key) ? this.boundaries.get(key) : null;
  }

  serialize(keys) {
    const out = {};
    for (const key of keys) {
      const t = this.tracks.get(key);
      if (!t) continue;
      const chunks = {};
      for (const [ci, arr] of t.chunks) chunks[ci] = f32ToB64(arr);
      out[key] = { q: t.q, hi: t.hi, chunks, b: this.boundaries.get(key) ?? null };
    }
    return out;
  }

  load(obj) {
    this.tracks.clear();
    this.boundaries.clear();
    for (const [key, t] of Object.entries(obj || {})) {
      // Projects saved before look changes stopped re-keying store template
      // results under `id@seed@x,y#look.rev-…`; fold those into the bare key
      // (the longest run wins when both exist).
      const bare = key.includes("#") ? key.slice(0, key.indexOf("#")) : key;
      const prev = this.tracks.get(bare);
      if (prev && prev.hi >= t.hi) continue;
      const chunks = new Map();
      for (const [ci, b64] of Object.entries(t.chunks)) chunks.set(Number(ci), b64ToF32(b64));
      this.tracks.set(bare, { q: t.q, hi: t.hi, chunks });
      if (t.b != null) this.boundaries.set(bare, t.b);
      else this.boundaries.delete(bare);
    }
    this._note(0);
  }
}

function f32ToB64(arr) {
  const bytes = new Uint8Array(arr.buffer, arr.byteOffset, arr.byteLength);
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  }
  return btoa(s);
}
function b64ToF32(b64) {
  const s = atob(b64);
  const bytes = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) bytes[i] = s.charCodeAt(i);
  return new Float32Array(bytes.buffer);
}
