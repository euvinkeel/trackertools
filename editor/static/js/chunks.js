// Per-frame float channels (e.g. bounds cx,cy,w,h or finetune dx,dy) stored in
// fixed-size chunks. NaN in channel 0 means "no value on this frame".
//
// Chunks are copy-on-write: snapshot() hands the current chunk arrays to the
// caller (the undo stack) and the store clones a chunk before its next write.
// Undo history therefore shares every chunk an edit didn't touch.

export const CHUNK_BITS = 8;
export const CHUNK = 1 << CHUNK_BITS;
const MASK = CHUNK - 1;

export class ChunkStore {
  constructor(channels) {
    this.channels = channels;
    this.chunks = new Map(); // chunk index -> Float32Array(CHUNK * channels)
    this.counts = new Map(); // chunk index -> number of frames with a value
    this.owned = new Set(); // chunks this store may mutate in place
    this.rev = 0;
  }

  _writable(ci) {
    let arr = this.chunks.get(ci);
    if (!arr) {
      arr = new Float32Array(CHUNK * this.channels).fill(NaN);
      this.chunks.set(ci, arr);
      this.counts.set(ci, 0);
      this.owned.add(ci);
    } else if (!this.owned.has(ci)) {
      arr = arr.slice();
      this.chunks.set(ci, arr);
      this.owned.add(ci);
    }
    return arr;
  }

  has(f) {
    const arr = this.chunks.get(f >> CHUNK_BITS);
    return !!arr && !Number.isNaN(arr[(f & MASK) * this.channels]);
  }

  // Values on frame f as an array, or null.
  get(f) {
    const arr = this.chunks.get(f >> CHUNK_BITS);
    if (!arr) return null;
    const o = (f & MASK) * this.channels;
    if (Number.isNaN(arr[o])) return null;
    return Array.from(arr.subarray(o, o + this.channels));
  }

  // values: array of `channels` numbers, or null to clear the frame.
  set(f, values) {
    if (f < 0) return;
    const ci = f >> CHUNK_BITS;
    if (values == null && !this.has(f)) return;
    const arr = this._writable(ci);
    const o = (f & MASK) * this.channels;
    const had = !Number.isNaN(arr[o]);
    for (let c = 0; c < this.channels; c++) arr[o + c] = values == null ? NaN : values[c];
    const has = values != null;
    if (had !== has) {
      const n = this.counts.get(ci) + (has ? 1 : -1);
      if (n === 0) {
        this.chunks.delete(ci);
        this.counts.delete(ci);
        this.owned.delete(ci);
      } else this.counts.set(ci, n);
    }
    this.rev++;
  }

  clearRange(a, b) {
    for (let f = Math.max(0, a); f < b; f++) {
      if (!this.chunks.has(f >> CHUNK_BITS)) {
        f = ((f >> CHUNK_BITS) + 1) * CHUNK - 1;
        continue;
      }
      this.set(f, null);
    }
  }

  get empty() {
    return this.chunks.size === 0;
  }

  // Last frame <= f with a value, or -1.
  prevDefined(f) {
    let ci = f >> CHUNK_BITS;
    let start = f & MASK;
    const keys = this._sortedChunks();
    while (ci >= 0) {
      const arr = this.chunks.get(ci);
      if (arr) {
        for (let i = start; i >= 0; i--) if (!Number.isNaN(arr[i * this.channels])) return ci * CHUNK + i;
      }
      const prev = lastBelow(keys, ci);
      if (prev < 0) return -1;
      ci = prev;
      start = MASK;
    }
    return -1;
  }

  // First frame >= f with a value, or -1.
  nextDefined(f) {
    let ci = f >> CHUNK_BITS;
    let start = f & MASK;
    const keys = this._sortedChunks();
    for (;;) {
      const arr = this.chunks.get(ci);
      if (arr) {
        for (let i = start; i < CHUNK; i++) if (!Number.isNaN(arr[i * this.channels])) return ci * CHUNK + i;
      }
      const next = firstAbove(keys, ci);
      if (next < 0) return -1;
      ci = next;
      start = 0;
    }
  }

  // [first, last] frames with values, or null.
  extent() {
    if (this.empty) return null;
    const keys = this._sortedChunks();
    return [this.nextDefined(keys[0] * CHUNK), this.prevDefined(keys[keys.length - 1] * CHUNK + MASK)];
  }

  _sortedChunks() {
    if (this._sortedRev !== this.rev || !this._sorted) {
      this._sorted = [...this.chunks.keys()].sort((a, b) => a - b);
      this._sortedRev = this.rev;
    }
    return this._sorted;
  }

  // ---- undo / persistence ----------------------------------------------------------

  snapshot() {
    this.owned.clear();
    return { channels: this.channels, chunks: new Map(this.chunks), counts: new Map(this.counts), rev: this.rev };
  }

  static fromSnapshot(snap) {
    const s = new ChunkStore(snap.channels);
    s.chunks = new Map(snap.chunks);
    s.counts = new Map(snap.counts);
    s.rev = snap.rev;
    return s;
  }

  serialize() {
    const chunks = {};
    for (const [ci, arr] of this.chunks) chunks[ci] = f32ToB64(arr);
    return { channels: this.channels, chunks };
  }

  static deserialize(obj) {
    const s = new ChunkStore(obj.channels);
    for (const [ci, b64] of Object.entries(obj.chunks || {})) {
      const arr = b64ToF32(b64);
      let n = 0;
      for (let i = 0; i < CHUNK; i++) if (!Number.isNaN(arr[i * s.channels])) n++;
      if (n) {
        s.chunks.set(Number(ci), arr);
        s.counts.set(Number(ci), n);
      }
    }
    return s;
  }
}

// Keyed-animation evaluation: linear between the surrounding keys, holding the
// first/last key outside them. Returns null when the store has no keys.
export function evalKeyed(store, f) {
  const a = store.prevDefined(f);
  if (a === f) return store.get(f);
  const b = store.nextDefined(f);
  if (a < 0 && b < 0) return null;
  if (a < 0) return store.get(b);
  if (b < 0) return store.get(a);
  const va = store.get(a);
  const vb = store.get(b);
  const t = (f - a) / (b - a);
  return va.map((v, i) => v + (vb[i] - v) * t);
}

function lastBelow(sorted, ci) {
  let lo = 0;
  let hi = sorted.length - 1;
  let ans = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (sorted[mid] < ci) {
      ans = sorted[mid];
      lo = mid + 1;
    } else hi = mid - 1;
  }
  return ans;
}

function firstAbove(sorted, ci) {
  let lo = 0;
  let hi = sorted.length - 1;
  let ans = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (sorted[mid] > ci) {
      ans = sorted[mid];
      hi = mid - 1;
    } else lo = mid + 1;
  }
  return ans;
}

export function f32ToB64(arr) {
  const bytes = new Uint8Array(arr.buffer, arr.byteOffset, arr.byteLength);
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  }
  return btoa(s);
}

export function b64ToF32(b64) {
  const s = atob(b64);
  const bytes = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) bytes[i] = s.charCodeAt(i);
  return new Float32Array(bytes.buffer);
}
