"""Template look correction + Go: the prefix must survive the subsequent run.

Run: pwvenv\\Scripts\\python.exe <this file>   (editor running on :8000)
"""
import json
import os
import sys
from pathlib import Path
from playwright.sync_api import sync_playwright

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
TMP = Path(os.environ.get("LOCALAPPDATA", "")) / "Temp" / "opencode"
CLIP = TMP / "cursor_clip.mp4"
TRUTH = json.loads((TMP / "cursor_truth.json").read_text())
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
    page.keyboard.press("n")
    page.keyboard.type("Cursor")
    page.keyboard.press("Enter")

    def ev(code, arg=None):
        return page.evaluate(code, arg) if arg is not None else page.evaluate(code)

    def at(x, y):
        return ev("([x,y]) => { const r=app.viewer.canvas.getBoundingClientRect(); const p=app.viewer.toScreen(x,y); return [r.left+p[0],r.top+p[1]]; }", [x, y])

    start, end = at(418, 312), at(438, 338)
    page.keyboard.down("Shift")
    page.mouse.move(*start)
    page.mouse.down()
    page.mouse.move(*end, steps=8)
    page.mouse.up()
    page.keyboard.up("Shift")
    page.wait_for_function("app.templateEditor.pixels != null", timeout=15000)
    page.locator('[data-action="save"]').click()
    page.locator("#btn-go").click()
    page.wait_for_function("['done','error','halted'].includes(app.tracker.state)", timeout=90000)
    check("first run done", ev("app.tracker.state") == "done", ev("app.tracker.status"))

    tid = ev("app.project.trackers[0].id")
    key = ev(f"app.project.segments(app.project.tracker({tid}))[0].key")
    check("tracked through the clip", ev(f"app.results.hi('{key}')") >= 599)
    sample_before = ev(f"[{{f: 0, v: app.results.get('{key}', 0)}}, {{f: 399, v: app.results.get('{key}', 399)}}, {{f: 399.0, mode: app.project.trackerState(app.project.tracker({tid}), 399, app.results).mode}}]")

    # Add a second look at frame 400 (the cursor after its teleport).
    ev("app.seek(400)")
    page.wait_for_function("app.shown === 400")
    ev(f"app.select({{subject: app.selSubject, tracker: {tid}}})")
    start, end = at(141, 714), at(161, 740)
    page.keyboard.down("Shift")
    page.mouse.move(*start)
    page.mouse.down()
    page.mouse.move(*end, steps=8)
    page.mouse.up()
    page.keyboard.up("Shift")
    page.wait_for_function("app.templateEditor.pixels != null", timeout=15000)
    page.locator('[data-action="save"]').click()
    check("look added", ev(f"app.project.tracker({tid}).looks.length") == 2)
    check("results cleared from the cursor", ev(f"app.results.hi('{key}')") == 399)
    check("boundary frozen at the cursor", ev(f"app.results.boundary('{key}')") == 400)

    # Go: warm-up from 399 must not overwrite the preserved prefix.
    ev("app.seek(400)")
    page.locator("#btn-go").click()
    page.wait_for_function("['done','error','halted'].includes(app.tracker.state)", timeout=90000)
    check("second run done", ev("app.tracker.state") == "done", ev("app.tracker.status"))
    check("re-tracked through the clip", ev(f"app.results.hi('{key}')") >= 599)
    sample_after = ev(f"[{{f: 0, v: app.results.get('{key}', 0)}}, {{f: 399, v: app.results.get('{key}', 399)}}]")
    check("frame 0 untouched", sample_after[0]["v"] == sample_before[0]["v"], (sample_before, sample_after))
    check("frame 399 (warm-up frame) untouched", sample_after[1]["v"] == sample_before[1]["v"], (sample_before[1], sample_after[1]))
    # The look slot is stable: historical frames still report look 0.
    st = ev(f"app.project.trackerState(app.project.tracker({tid}), 100, app.results)")
    check("historical look slot preserved", st["look"] == 0 and st["lookIndex"] == 0, st)

    # Save/reload keeps the boundary so a later Go cannot rewrite the prefix.
    ev("app.save()")
    page.wait_for_function("app.meta && !app.dirty", timeout=15000)
    saved = ev(f"app.results.serialize(app.project.resultKeys(app.results))['{key}'].b")
    check("boundary is persisted", saved == 400, saved)

    # Undo/redo still restore the cleared results.
    ev("app.undo()")
    check("undo restores the look set", ev(f"app.project.tracker({tid}).looks.length") == 1)
    check("undo removes the boundary", ev(f"app.results.boundary('{key}')") is None)
    ev("app.redo()")
    check("redo restores the boundary", ev(f"app.results.boundary('{key}')") == 400)

    check("0 console errors", not errors, errors[:3])
    browser.close()

passed = sum(1 for _, ok in checks if ok)
print(f"{passed}/{len(checks)} checks passed")
sys.exit(0 if passed == len(checks) else 1)
