async function json(res) {
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`;
    try {
      const body = await res.json();
      if (body.detail) msg = body.detail;
    } catch {}
    const err = new Error(msg);
    err.status = res.status;
    throw err;
  }
  return res.json();
}

export const api = {
  health: () => fetch("/api/health").then(json),
  videos: () => fetch("/api/videos").then(json),
  video: (id) => fetch(`/api/videos/${id}`).then(json),
  fileUrl: (id) => `/api/videos/${id}/file`,
  cropUrl: (id, f, x, y, w, h) => `/api/videos/${id}/crop?${new URLSearchParams({
    f: Math.round(f), x: Math.round(x), y: Math.round(y), w: Math.round(w), h: Math.round(h),
  })}`,
  proxyUrl: (id) => `/api/videos/${id}/proxy/file`,
  proxyStatus: (id) => fetch(`/api/videos/${id}/proxy/status`).then(json),

  lookup: (fingerprint) => fetch(`/api/videos/lookup?fingerprint=${encodeURIComponent(fingerprint)}`).then(json),

  openPath: (path) =>
    fetch("/api/videos/open", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path }),
    }).then(json),

  // Streams the file to the server (XHR gives upload progress; the browser
  // reads from disk, so large files don't load into memory).
  upload(file, fingerprint, onProgress) {
    return new Promise((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      const qs = `name=${encodeURIComponent(file.name)}&fingerprint=${encodeURIComponent(fingerprint)}`;
      xhr.open("POST", `/api/videos/upload?${qs}`);
      xhr.setRequestHeader("Content-Type", "application/octet-stream");
      xhr.upload.onprogress = (e) => e.lengthComputable && onProgress(e.loaded / e.total);
      xhr.onload = () => {
        let body = null;
        try {
          body = JSON.parse(xhr.responseText);
        } catch {}
        if (xhr.status >= 200 && xhr.status < 300) resolve(body);
        else reject(new Error(body?.detail || `Upload failed (${xhr.status})`));
      };
      xhr.onerror = () => reject(new Error("Upload failed (network error)"));
      xhr.send(file);
    });
  },

  loadProject: (id) => fetch(`/api/projects/${id}`).then(json),

  saveProject: (id, text) =>
    fetch(`/api/projects/${id}`, { method: "PUT", headers: { "Content-Type": "application/json" }, body: text }).then(json),
};
