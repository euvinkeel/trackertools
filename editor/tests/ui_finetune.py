"""Finetune layers (R), automatic ends and end-marker editing (Playwright)."""
import os
import sys
from pathlib import Path
from playwright.sync_api import sync_playwright

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
TMP = Path(os.environ.get("LOCALAPPDATA", "")) / "Temp" / "opencode"
CLIP = TMP / "cursor_clip.mp4"
errors = []
checks = []


def check(name, ok, detail=None):
    checks.append((name, ok))
    print(("PASS " if ok else "FAIL ") + name, "" if ok else (detail or ""))


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome", headless=True)
    page = browser.new_page(viewport={"width": 1600, "height": 950})
    page.on("pageerror", lambda err: errors.append(str(err)))
    page.on("console", lambda msg: errors.append(msg.text) if msg.type == "error" else None)
    page.goto("http://127.0.0.1:8000")
    page.set_input_files("#file-input", str(CLIP))
    page.wait_for_function("app.ready && app.shown >= 0", timeout=60000)

    def ev(code, arg=None):
        return page.evaluate(code, arg) if arg is not None else page.evaluate(code)

    ev("app.newSubject()")
    page.keyboard.press("Escape")
    sid = ev("app.selSubject")
    ev(f"app.edit('p', () => {{ app.project.addPoint({sid}, 0, 400, 400, true); app.project.addPoint({sid}, 0, 500, 500, true); }})")
    ev("app.seek(0)")
    page.wait_for_function("app.shown === 0")

    # R enters finetune mode and creates a layer.
    page.keyboard.press("r")
    check("R enters finetune", ev("app.viewer.mode.name") == "finetune")
    check("a layer was created", ev(f"app.project.subject({sid}).layers.length") == 1)
    layer = ev(f"app.project.subject({sid}).layers[0].id")

    # Drag: relative pointer-lock movement in source pixels.
    p0 = ev("(() => { const p = app.viewer.toScreen(450, 450); const r = app.viewer.canvas.getBoundingClientRect(); return [r.left + p[0], r.top + p[1]]; })()")
    scale = ev("app.viewer.s")
    page.mouse.move(*p0)
    page.mouse.down()
    page.mouse.move(p0[0] + 60, p0[1] + 30, steps=6)
    page.mouse.up()
    keys = ev(f"(() => {{ const l = app.project.subject({sid}).layers[0]; const st = app.project.store(l.keys); return st ? [st.get(0), st.extent()] : null; }})()")
    check("drag keys the layer", keys and keys[0] and abs(keys[0][0] - 60 / scale) < 1.5 and abs(keys[0][1] - 30 / scale) < 1.5, (keys, scale))
    st = ev(f"app.project.subjectState(app.project.subject({sid}), 0, app.results)")
    check("final = pushed + finetune", abs(st["fineX"] - 60 / scale) < 1.5 and abs(st["x"] - (st["rawX"] + st["fineX"])) < 1e-6, st)

    # Shift is 1/10 speed.
    before = keys[0][0]
    page.mouse.move(*p0)
    page.mouse.down()
    page.keyboard.down("Shift")
    page.mouse.move(p0[0] + 100, p0[1], steps=6)
    page.mouse.up()
    page.keyboard.up("Shift")
    after = ev(f"app.project.store(app.project.subject({sid}).layers[0].keys).get(0)")[0]
    check("Shift is 1/10 speed", abs((after - before) - 10 / scale) < 1.5, (before, after, scale))

    # Arrow stepping while dragging writes a held key on the stepped frame.
    page.mouse.move(*p0)
    page.mouse.down()
    page.mouse.move(p0[0] + 20, p0[1], steps=3)
    page.keyboard.press("ArrowRight")
    page.keyboard.press("ArrowRight")
    page.mouse.up()
    ext = ev(f"app.project.store(app.project.subject({sid}).layers[0].keys).extent()")
    check("arrow stepping holds the value forward", ext[1] == 2, ext)
    page.keyboard.press("ArrowLeft")
    page.keyboard.press("ArrowLeft")
    check("the session stepped back", ev("app.cursor") == 0)

    # Weight 0 removes the layer; undo/redo walks the edits back.
    ev(f"app.setLayer({sid}, {layer}, {{ weight: 0 }})")
    st = ev(f"app.project.subjectState(app.project.subject({sid}), 0, app.results)")
    check("weight 0 removes the layer", abs(st["fineX"]) < 1e-6, st)
    ev(f"app.setLayer({sid}, {layer}, {{ weight: 1 }})")
    ev("app.undo()")
    ev("app.undo()")
    ev("app.undo()")
    v_after_undo = ev(f"app.project.store(app.project.subject({sid}).layers[0].keys).get(2)")
    check("undo removes the arrow-step key", v_after_undo is None, v_after_undo)
    check("undo restores the weight", ev(f"app.project.subject({sid}).layers[0].weight") == 1)
    ev("app.redo()")
    check("redo restores the key", ev(f"app.project.store(app.project.subject({sid}).layers[0].keys).get(2)") is not None)

    # Q loupe.
    page.keyboard.down("q")
    check("Q shows the loupe", ev("app.viewer.finetune.loupe") is True)
    page.keyboard.up("q")
    check("releasing Q hides it", ev("app.viewer.finetune.loupe") is False)
    page.keyboard.press("r")
    check("R exits finetune", ev("app.viewer.mode.name") == "normal")

    # ---- automatic ends -----------------------------------------------------
    # An auto-tracked point that is inside the bounds until frame 50, then far out.
    tid = ev(f"(() => {{ const p = app.project.addPoint({sid}, 0, 100, 100); return p.id; }})()")
    key = ev(f"app.project.segments(app.project.tracker({tid}))[0].key")
    ev(f"""(() => {{
      const N = app.meta.frameCount; const d = new Float32Array(N * 3);
      for (let f = 0; f < N; f++) {{ d[3*f] = f < 50 ? 100 : 1000; d[3*f+1] = 100; d[3*f+2] = 1; }}
      app.results.write('{key}', 0, 0, d);
      const s = app.project.subject({sid});
      const store = app.project.ensureBounds(s.id);
      for (let f = 0; f < N; f++) store.set(f, [100, 100, 80, 80]);
      app.project.touch(0);
      app.setDriftPolicy(s.id, 'end');
    }})()""")
    page.wait_for_timeout(600)
    auto = ev(f"app.project.tracker({tid}).autoEnd")
    check("automatic end confirmed", auto and auto["reason"] == "bounds" and auto["f"] == 50, auto)
    check("effective end is the first escaped frame", ev(f"app.project.end(app.project.tracker({tid}))") == 50)
    check("frames before the end stay tracked", ev(f"app.project.trackerState(app.project.tracker({tid}), 49, app.results).mode") == "auto")
    ev(f"app.select({{subject: {sid}, tracker: {tid}}})")
    page.keyboard.press("u")
    check("U removes the automatic end and disables auto",
          ev(f"app.project.tracker({tid}).autoEnd") is None and ev(f"app.project.tracker({tid}).noAutoEnd") is True)
    check("later results are available again", ev(f"app.project.trackerState(app.project.tracker({tid}), 100, app.results).mode") == "auto")
    ev(f"app.setAutoEnd({tid}, true)")
    page.wait_for_timeout(600)
    check("re-enabling restores the automatic end", ev(f"app.project.tracker({tid}).autoEnd.f") == 50)

    # ---- timeline end-marker focus + Delete ---------------------------------
    ev(f"app.setTrackerEnd({tid}, 300)")
    ev("app.seek(0)")
    marker = ev(f"""(() => {{
      const t = app.timeline; const r = t.canvas.getBoundingClientRect();
      const row = t.rows().find(q => q.kind === 'tracker' && q.p.id === {tid});
      const y = t.top + row.y - t.scrollY + row.h / 2;
      return [r.left + t.x(300), r.top + y];
    }})()""")
    page.mouse.click(*marker)
    check("clicking the end marker focuses it", ev("app.endMarkerFocus") == tid)
    page.keyboard.press("Delete")
    check("Delete removes the end, not the tracker",
          ev(f"app.project.tracker({tid}) != null") and ev(f"app.project.end(app.project.tracker({tid}))") == 600)
    ev("app.undo()")
    check("undo restores the end", ev(f"app.project.end(app.project.tracker({tid}))") == 300)

    check("0 console errors", not errors, errors[:3])
    browser.close()

passed = sum(1 for _, ok in checks if ok)
print(f"{passed}/{len(checks)} checks passed")
sys.exit(0 if passed == len(checks) else 1)
