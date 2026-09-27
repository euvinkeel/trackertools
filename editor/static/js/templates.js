// Shared template result and mask format (mirrors editor/templates.py).
// Packed results identify a look by its stable slot (never its array index).
export function unpackMatch(v) {
  const slot = Math.floor(v / 4);
  return { slot, score: v - slot * 4 - 1 };
}

export function encodeMask(mask) {
  let binary = "";
  for (let i = 0; i < mask.length; i += 0x8000) {
    binary += String.fromCharCode(...mask.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}

export function decodeMask(encoded, w, h) {
  if (!encoded) return new Uint8Array(w * h).fill(1);
  const binary = atob(encoded);
  if (binary.length !== w * h) return new Uint8Array(w * h).fill(1);
  return Uint8Array.from(binary, (c) => c.charCodeAt(0) ? 1 : 0);
}

// Flood the border through similar neighbouring colors; everything the flood
// cannot reach is the foreground. Useful for outlined cursors on footage.
export function maskFromBorder(rgba, w, h, tolerance = 28) {
  const out = new Uint8Array(w * h).fill(1);
  const visited = new Uint8Array(w * h);
  const queue = new Int32Array(w * h);
  let head = 0, tail = 0;
  const add = (i) => { if (!visited[i]) { visited[i] = 1; queue[tail++] = i; } };
  for (let x = 0; x < w; x++) { add(x); add((h - 1) * w + x); }
  for (let y = 0; y < h; y++) { add(y * w); add(y * w + w - 1); }
  const limit = tolerance * 3;
  while (head < tail) {
    const i = queue[head++], x = i % w, y = Math.floor(i / w);
    out[i] = 0;
    for (const j of [x ? i - 1 : -1, x + 1 < w ? i + 1 : -1, y ? i - w : -1, y + 1 < h ? i + w : -1]) {
      if (j < 0 || visited[j]) continue;
      const a = i * 4, b = j * 4;
      if (Math.abs(rgba[a] - rgba[b]) + Math.abs(rgba[a + 1] - rgba[b + 1]) + Math.abs(rgba[a + 2] - rgba[b + 2]) <= limit) add(j);
    }
  }
  // If the outline is open, prefer the largest connected foreground island.
  const seen = new Uint8Array(w * h);
  let best = [];
  for (let i = 0; i < out.length; i++) {
    if (!out[i] || seen[i]) continue;
    const island = [i]; seen[i] = 1;
    for (let k = 0; k < island.length; k++) {
      const p = island[k], x = p % w, y = Math.floor(p / w);
      for (const j of [x ? p - 1 : -1, x + 1 < w ? p + 1 : -1, y ? p - w : -1, y + 1 < h ? p + w : -1]) {
        if (j >= 0 && out[j] && !seen[j]) { seen[j] = 1; island.push(j); }
      }
    }
    if (island.length > best.length) best = island;
  }
  out.fill(0);
  for (const i of best) out[i] = 1;
  return out;
}
