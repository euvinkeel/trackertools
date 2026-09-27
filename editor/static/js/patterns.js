// A library of reusable template patterns: the box's pixels, mask and hotspot,
// kept in browser storage so a pattern can be applied to any template tracker in
// any video. A pattern is a few KB (a small PNG), so localStorage is enough; the
// library is capped and every failure is reported to the caller as an Error.
//
// Entry: { id, name, w, h, hx, hy, tmpl, mask, created }
//   tmpl  base64 PNG of the box at original resolution (no "data:" prefix)
//   mask  base64 of w*h bytes (1 = used), "" = the whole box

const KEY = "cotrack.patterns.v1";
export const MAX_PATTERNS = 200;
export const MAX_BYTES = 4_000_000;
export const NAME_MAX = 40;

const memoryData = new Map();
const memoryStorage = {
  getItem: (k) => (memoryData.has(k) ? memoryData.get(k) : null),
  setItem: (k, v) => memoryData.set(k, String(v)),
  removeItem: (k) => memoryData.delete(k),
};
let chosen = null;
let persistent = true;

// localStorage when it works (browsers), an in-memory shim otherwise (tests,
// private mode, storage disabled). The shim is temporary: the UI labels it.
function storage() {
  if (chosen) return chosen;
  try {
    const ls = globalThis.localStorage;
    if (!ls) throw new Error("no localStorage");
    const probe = `${KEY}.probe`;
    ls.setItem(probe, "1");
    ls.removeItem(probe);
    chosen = ls;
    persistent = true;
  } catch {
    chosen = memoryStorage;
    persistent = false;
  }
  return chosen;
}

// False when the library lives only in memory for this session.
export function isPersistent() {
  storage();
  return persistent;
}

function valid(p) {
  return p && typeof p === "object" && typeof p.name === "string" && typeof p.tmpl === "string"
    && Number.isFinite(p.w) && Number.isFinite(p.h) && p.w >= 4 && p.h >= 4 && p.w <= 512 && p.h <= 512
    && Number.isFinite(p.hx) && Number.isFinite(p.hy);
}

export function listPatterns() {
  try {
    const raw = storage().getItem(KEY);
    const arr = raw ? JSON.parse(raw) : [];
    return Array.isArray(arr) ? arr.filter(valid) : [];
  } catch {
    return [];
  }
}

export function patternById(id) {
  return listPatterns().find((p) => p.id === id) || null;
}

// Approximate storage used by the library, in characters (roughly bytes).
export function patternBytes() {
  try {
    return (storage().getItem(KEY) || "").length;
  } catch {
    return 0;
  }
}

function write(all) {
  const json = JSON.stringify(all);
  if (all.length > MAX_PATTERNS) throw new Error(`The library holds at most ${MAX_PATTERNS} patterns — delete a few first.`);
  if (json.length > MAX_BYTES) throw new Error("The pattern library is full — delete a few patterns first.");
  try {
    storage().setItem(KEY, json);
  } catch {
    throw new Error("Couldn't save the pattern: browser storage is full or unavailable.");
  }
  return all;
}

// Save (or replace, when the name matches) a pattern. Throws on a bad name, a
// full store, or unavailable storage.
export function savePattern({ id, name, w, h, hx, hy, tmpl, mask }) {
  const trimmed = String(name || "").trim().slice(0, NAME_MAX);
  if (!trimmed) throw new Error("Give the pattern a name first.");
  if (typeof tmpl !== "string" || !tmpl) throw new Error("The pattern has no pixels yet.");
  const all = listPatterns();
  const entry = {
    id: id || `pat${Date.now().toString(36)}${Math.floor(Math.random() * 46656).toString(36)}`,
    name: trimmed, w, h, hx, hy, tmpl, mask: mask || "", created: Date.now(),
  };
  const byId = all.findIndex((p) => p.id === entry.id);
  const byName = all.findIndex((p) => p.name.toLowerCase() === trimmed.toLowerCase());
  const i = byId >= 0 ? byId : byName;
  if (i >= 0) {
    entry.id = all[i].id;
    entry.created = all[i].created ?? entry.created;
    all[i] = entry;
  } else {
    all.push(entry);
  }
  write(all);
  return entry;
}

export function renamePattern(id, name) {
  const trimmed = String(name || "").trim().slice(0, NAME_MAX);
  if (!trimmed) throw new Error("Give the pattern a name first.");
  const all = listPatterns();
  const p = all.find((x) => x.id === id);
  if (!p) throw new Error("That pattern no longer exists.");
  p.name = trimmed;
  write(all);
  return p;
}

export function deletePattern(id) {
  const all = listPatterns().filter((p) => p.id !== id);
  try {
    write(all);
  } catch {
    /* deleting never grows the store */
  }
  return all;
}

export function clearPatterns() {
  try {
    storage().removeItem(KEY);
  } catch {
    /* nothing to do */
  }
}

// ---- sharing between browsers/machines -------------------------------------

export function exportPatterns() {
  return JSON.stringify({ format: "cotrack.patterns", version: 1, patterns: listPatterns() }, null, 1);
}

// Merge an exported library (replace=false keeps existing entries by name).
export function importPatterns(text, { replace = false } = {}) {
  let data;
  try {
    data = JSON.parse(text);
  } catch {
    throw new Error("That file is not valid JSON.");
  }
  const incoming = Array.isArray(data) ? data : data?.patterns;
  if (!Array.isArray(incoming)) throw new Error("That file doesn't contain a pattern library.");
  const all = replace ? [] : listPatterns();
  let added = 0;
  for (const raw of incoming) {
    if (!valid(raw)) continue;
    const name = String(raw.name).trim().slice(0, NAME_MAX) || "Imported pattern";
    const i = all.findIndex((p) => p.name.toLowerCase() === name.toLowerCase());
    const entry = {
      id: `pat${Date.now().toString(36)}${Math.floor(Math.random() * 46656).toString(36)}${added}`,
      name, w: raw.w, h: raw.h, hx: raw.hx, hy: raw.hy, tmpl: raw.tmpl, mask: raw.mask || "", created: Date.now(),
    };
    if (i >= 0) all[i] = entry;
    else all.push(entry);
    added++;
  }
  write(all);
  return { added, total: all.length };
}
