"""End-to-end template flow on the synthetic cursor clip (Playwright)."""
import json
import os
import sys
from pathlib import Path
from playwright.sync_api import sync_playwright

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
TMP = Path(os.environ.get("LOCALAPPDATA", "")) / "Temp" / "opencode"
CLIP = TMP / "cursor_clip.mp4"
TRUTH = json.loads((TMP / "cursor_truth.json").read_text())
assert CLIP.exists()
errors = []

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

    def at(x, y):
        return page.evaluate("([x,y]) => { const r=app.viewer.canvas.getBoundingClientRect(); const p=app.viewer.toScreen(x,y); return [r.left+p[0],r.top+p[1]]; }", [x, y])

    # Sprite spans 16×22 px; the tip is at (420,314). Include a 2 px border.
    start, end = at(418, 312), at(438, 338)
    page.keyboard.down("Shift")
    page.mouse.move(*start)
    page.mouse.down()
    page.mouse.move(*end, steps=8)
    page.mouse.up()
    page.keyboard.up("Shift")
    page.locator("#template-editor canvas").wait_for(state="visible")
    page.wait_for_function("app.templateEditor.pixels != null", timeout=15000)
    page.locator('[name="tool"][value="hotspot"]').check()
    box = page.locator("#template-editor canvas").bounding_box()
    page.mouse.click(box["x"] + 2.5 / 20 * box["width"], box["y"] + 2.5 / 26 * box["height"])
    page.locator('[data-action="auto"]').click()
    page.locator('[data-action="save"]').click()
    data = page.evaluate("({p:app.project.trackers[0], selection:app.selTracker})")
    assert data["p"]["kind"] == "template" and len(data["p"]["looks"]) == 1, data
    assert data["p"]["looks"][0]["mask"], "Auto mask should select some of the sprite"
    page.locator("#btn-go").click()
    try:
        page.wait_for_function("['done','error','halted'].includes(app.tracker.state)", timeout=45000)
    except Exception:
        print("TIMEOUT", page.evaluate("({run:app.tracker.run,state:app.tracker.state,status:app.tracker.status,hi:app.results.hi(app.project.segments(app.project.trackers[0])[0].key),errors:document.querySelector('#tracker-stats').textContent})"), errors)
        raise
    assert page.evaluate("app.tracker.state") == "done", page.evaluate("app.tracker.status")
    samples = page.evaluate("""() => {const p=app.project.trackers[0]; return [1,20,100,299,305,329,330,400,599].map(f=>({f,...app.project.trackerState(p,f,app.results)}));} """)
    print("Template samples:", [(s["f"], s["mode"], s.get("lost"), round(s.get("score", 0), 2), round(s["x"], 1), round(s["y"], 1)) for s in samples])
    assert samples[4]["lost"] and samples[5]["lost"], "hidden cursor must be not found"
    assert all(not samples[i]["lost"] for i in [0, 1, 2, 3, 6, 7, 8]), "visible cursor must be found"
    assert max(abs(samples[i]["x"] - (TRUTH["tips"][samples[i]["f"]][0] + .5)) for i in [0,1,2,3,6,7,8]) < 2
    page.evaluate("app.seek(305)")
    page.wait_for_function("app.shown === 305")
    page.locator("#point-panel .badge.lost").wait_for(timeout=5000)

    # ---- adding a look applies from the cursor on, not to the whole tracker ----
    key = page.evaluate("app.project.segments(app.project.trackers[0])[0].key")
    before = page.evaluate(f"app.results.hi('{key}')")
    page.evaluate("app.seek(400)")
    page.wait_for_function("app.shown === 400")
    page.evaluate("app.select({subject: app.selSubject, tracker: app.project.trackers[0].id})")
    start, end = at(141, 714), at(161, 740)  # the sprite after the teleport
    page.keyboard.down("Shift")
    page.mouse.move(*start)
    page.mouse.down()
    page.mouse.move(*end, steps=8)
    page.mouse.up()
    page.keyboard.up("Shift")
    page.wait_for_function("app.templateEditor.pixels != null", timeout=15000)
    page.locator('[data-action="save"]').click()
    assert page.evaluate("app.project.trackers[0].looks.length") == 2, "look added"
    hi = page.evaluate(f"app.results.hi('{key}')")
    assert hi == 399, f"results must be cleared only from the cursor on (hi={hi}, was {before})"
    assert page.evaluate(f"app.results.get('{key}', 399) != null"), "frames before the cursor stay"
    assert page.evaluate(f"app.results.get('{key}', 401) == null"), "frames after the cursor are cleared"
    assert page.evaluate("app.project.trackerState(app.project.trackers[0], 401, app.results).mode") == "pending"
    page.keyboard.press("Control+z")
    assert page.evaluate(f"app.results.hi('{key}')") == before, "undo brings the cleared results back"
    assert page.evaluate("app.project.trackers[0].looks.length") == 1, "undo removes the look"
    assert not errors, errors
    print("Template UI: PASS (0 console errors)")
    browser.close()
