"""Pattern library + bounds passes + reversible ends (Playwright).

Run: pwvenv\\Scripts\\python.exe editor\\tests\\ui_library.py   (editor on :8000)
"""
import os
import sys
from pathlib import Path
from playwright.sync_api import sync_playwright

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
TMP = Path(os.environ.get("LOCALAPPDATA", "")) / "Temp" / "opencode"
CLIP = TMP / "cursor_clip.mp4"
errors = []
bad = []
checks = []


def check(name, ok, detail=None):
    checks.append((name, ok))
    print(("PASS " if ok else "FAIL ") + name, "" if ok else (detail or ""))


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome", headless=True)
    page = browser.new_page(viewport={"width": 1600, "height": 950})
    page.on("pageerror", lambda err: errors.append(str(err)))
    page.on("console", lambda msg: errors.append(msg.text) if msg.type == "error" else None)
    page.on("response", lambda r: bad.append((r.status, r.url)) if r.status >= 400 else None)
    page.goto("http://127.0.0.1:8000")
    page.set_input_files("#file-input", str(CLIP))
    page.wait_for_function("app.ready && app.shown >= 0", timeout=60000)
    page.keyboard.press("n")
    page.keyboard.type("Cursor")
    page.keyboard.press("Enter")

    def ev(code, arg=None):
        return page.evaluate(code, arg) if arg is not None else page.evaluate(code)

    def at(x, y):
        return ev("([x,y]) => { const r=app.viewer.canvas.getBoundingClientRect(); const p=app.viewer.toScreen(x,y); return [r.left+p[0],r.top+p[1]]; }", [x, y])

    # ---- pattern library: create, save, use ---------------------------------
    start, end = at(418, 312), at(438, 338)
    page.keyboard.down("Shift")
    page.mouse.move(*start)
    page.mouse.down()
    page.mouse.move(*end, steps=8)
    page.mouse.up()
    page.keyboard.up("Shift")
    page.locator("#template-editor canvas").wait_for(state="visible")
    page.wait_for_function("app.templateEditor.pixels != null", timeout=15000)
    page.locator('[name="lib-name"]').fill("Cursor sprite")
    page.locator('[data-action="save-lib"]').click()
    check("pattern saved to the library", ev("app.sidebar.patternCount()") == 1)
    page.locator("#pattern-panel .pattern").wait_for(timeout=5000)
    check("pattern thumbnail shown", ev("!!document.querySelector('#pattern-panel .pattern img').src.startsWith('data:image/png')"))
    page.locator('[data-action="save"]').click()
    check("tracker created", ev("app.project.trackers[0].kind") == "template")

    page.evaluate("app.seek(0)")
    page.locator("#btn-go").click()
    page.wait_for_function("['done','error','halted'].includes(app.tracker.state)", timeout=60000)
    check("template tracked", ev("app.tracker.state") == "done", ev("app.tracker.status"))

    # Use the pattern as a second look from frame 400 (undoable, pixels copied).
    ev("app.seek(400); app.select({subject: app.selSubject, tracker: app.project.trackers[0].id})")
    page.wait_for_function("app.shown === 400")
    page.locator('#pattern-panel [data-act="use-pattern"]').click()
    check("pattern applied as a look", ev("app.project.trackers[0].looks.length") == 2)
    check("pixels copied into the look", ev("app.project.trackers[0].looks[1].tmpl.length > 0"))
    check("look slot is stable", ev("app.project.trackers[0].looks[1].slot") == 1)
    page.keyboard.press("Control+z")
    check("undo removes the applied look", ev("app.project.trackers[0].looks.length") == 1)

    # Export/import round trip through the UI's helpers.
    text = ev("import('/static/js/patterns.js').then(m => m.exportPatterns())")
    check("library exports JSON", '"cotrack.patterns"' in text)

    # ---- reversible ends ----------------------------------------------------
    key = ev("app.project.segments(app.project.trackers[0])[0].key")
    hi = ev(f"app.results.hi('{key}')")
    ev("app.setTrackerEnd(app.project.trackers[0].id, 300)")
    check("end moved to 300", ev("app.project.end(app.project.trackers[0])") == 300)
    check("frames before the end stay", ev("app.project.trackerState(app.project.trackers[0], 299, app.results).mode") == "auto")
    check("frames after the end are dormant", ev("app.project.trackerState(app.project.trackers[0], 300, app.results)") is None)
    check("ending keeps the stored results", ev(f"app.results.hi('{key}')") == hi)
    ev("app.removeEnds([app.project.trackers[0]], 'test')")
    check("remove end restores the tracker", ev("app.project.end(app.project.trackers[0])") == 600)
    check("results beyond the end come back", ev("app.project.trackerState(app.project.trackers[0], 500, app.results).mode") == "auto")

    # ---- bounds passes ------------------------------------------------------
    ev("""
      () => {
        const s = app.project.subject(app.selSubject);
        const n = 100;
        const boxes = new Float32Array(n * 4);
        for (let i = 0; i < n; i++) { boxes[4*i] = 100 + i; boxes[4*i+1] = 100; boxes[4*i+2] = 40; boxes[4*i+3] = 40; }
        const samples = [];
        for (let f = 0; f <= 99; f += 0.5) samples.push({ u: f, x: 100 + f, y: 100 });
        app.addBoundsPass(s.id, { name: "Test pass", level: 0, kind: "refine", a: 0, b: 99, samples, settings: {}, boxes });
      }
    """)
    check("pass recorded", ev("app.project.subject(app.selSubject).boundsPasses.length") == 1)
    b = ev("app.project.boundsAt(app.project.subject(app.selSubject), 50)")
    check("composite bounds follow the pass", b is not None and abs(b[0] - 150) < 2, b)
    ev("""
      () => {
        const s = app.project.subject(app.selSubject);
        app.editBounds("test nudge", s.id, (store) => {
          const eff = app.project.boundsAt(s, 50);
          const v = eff.slice(); v[0] += 30; store.set(50, v); return [50, 50];
        });
      }
    """)
    check("manual layer overrides the pass", abs(ev("app.project.boundsAt(app.project.subject(app.selSubject), 50)[0]") - 180) < 2)
    # The pass (settings + boxes + samples) survives a save/reload.
    ev("app.save()")
    page.wait_for_function("app.meta && !app.dirty", timeout=15000)
    page.reload()
    page.set_input_files("#file-input", str(CLIP))
    page.wait_for_function("app.ready && app.shown >= 0", timeout=60000)
    check("pass survives a reload", ev("app.project.subject(app.selSubject).boundsPasses.length") == 1)
    n = ev("app.project.subject(app.selSubject).boundsPasses[0].samples.length")
    check("samples survive for regeneration", n == 199, n)
    b2 = ev("app.project.boundsAt(app.project.subject(app.selSubject), 50)")
    check("composite survives a reload", b2 is not None and abs(b2[0] - 180) < 2, b2)
    ev("app.removeBoundsPass(app.selSubject, app.project.subject(app.selSubject).boundsPasses[0].id)")
    check("pass deleted", ev("app.project.subject(app.selSubject).boundsPasses.length") == 0)

    check("no failed requests", not bad, bad[:3])
    check("0 console errors", not errors, errors[:3])
    browser.close()

passed = sum(1 for _, ok in checks if ok)
print(f"{passed}/{len(checks)} checks passed")
sys.exit(0 if passed == len(checks) else 1)
