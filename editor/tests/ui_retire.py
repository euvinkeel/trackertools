"""Live retirement: a retire message stops only the named segments at their
cutoff while other trackers finish (Playwright)."""
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

    def ev(code):
        return page.evaluate(code)

    ev("app.newSubject()")
    page.keyboard.press("Escape")
    ev("""
      () => {
        const s = app.project.subject(app.selSubject);
        app.project.addPoint(s.id, 0, 700, 400);
        app.project.addPoint(s.id, 0, 900, 400);
      }
    """)
    keys = ev("app.project.trackers.map(t => app.project.segments(t)[0].key)")
    page.locator("#btn-go").click()
    page.wait_for_function("app.tracker.state === 'running'", timeout=60000)
    # Retire the first point at frame 60 while the run is live.
    ev(f"app.tracker.retire([{{ key: '{keys[0]}', frame: 60 }}])")
    page.wait_for_function("['done','error','halted'].includes(app.tracker.state)", timeout=120000)
    check("run finished", ev("app.tracker.state") == "done", ev("app.tracker.status"))
    hi0 = ev(f"app.results.hi('{keys[0]}')")
    hi1 = ev(f"app.results.hi('{keys[1]}')")
    check("retired segment stopped near its cutoff", 50 <= hi0 <= 80, hi0)
    check("the other segment finished the clip", hi1 >= 599, hi1)
    check("effective cutoff wins over archived frames", ev(f"app.results.boundary('{keys[0]}')") is None)
    check("0 console errors", not errors, errors[:3])
    browser.close()

passed = sum(1 for _, ok in checks if ok)
print(f"{passed}/{len(checks)} checks passed")
sys.exit(0 if passed == len(checks) else 1)
