"""End-to-end bounds flow on the synthetic cursor clip (Playwright): draw bounds,
undoable invalidation, a scripted Puppeteer pass, drift + End drifted, template
search inside bounds, falloff nudge, export."""
import json
import os
import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
TMP = Path(os.environ.get("LOCALAPPDATA", "")) / "Temp" / "opencode"
CLIP = TMP / "cursor_clip.mp4"
TRUTH = json.loads((TMP / "cursor_truth.json").read_text())
TIPS = TRUTH["tips"]
errors, checks = [], []


def check(name, cond, detail=""):
    checks.append(bool(cond))
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if detail else ""))


with sync_playwright() as pw:
    browser = pw.chromium.launch(channel="chrome", headless=True)
    page = browser.new_page(viewport={"width": 1600, "height": 950})
    page.on("pageerror", lambda err: errors.append(str(err)))
    page.on("console", lambda msg: errors.append(msg.text) if msg.type == "error" else None)
    page.goto("http://127.0.0.1:8000")
    page.set_input_files("#file-input", str(CLIP))
    page.wait_for_function("app.ready && app.shown >= 0", timeout=60000)
    ev = page.evaluate

    def at(x, y):
        return ev("([x,y]) => { const r=app.viewer.canvas.getBoundingClientRect(); const p=app.viewer.toScreen(x,y); return [r.left+p[0],r.top+p[1]]; }", [x, y])

    def seek(f):
        ev(f"app.seek({f})")
        page.wait_for_function(f"app.shown === {f}")

    def drag(a, b, steps=8):
        page.mouse.move(*a)
        page.mouse.down()
        page.mouse.move(*b, steps=steps)
        page.mouse.up()

    def go():
        seek(0)
        page.locator("#btn-go").click()
        page.wait_for_function("['done','error','halted'].includes(app.tracker.state)", timeout=120000)

    page.keyboard.press("n")
    page.keyboard.type("Cursor")
    page.keyboard.press("Enter")
    # Template tracker on the cursor (hotspot at the tip) + a CoTracker point far away.
    page.keyboard.down("Shift")
    drag(at(418, 312), at(438, 338))
    page.keyboard.up("Shift")
    page.wait_for_function("app.templateEditor.pixels != null", timeout=15000)
    page.locator('[name="tool"][value="hotspot"]').check()
    box = page.locator("#template-editor canvas").bounding_box()
    page.mouse.click(box["x"] + 2.5 / 20 * box["width"], box["y"] + 2.5 / 26 * box["height"])
    page.locator('[data-action="save"]').click()
    tid = ev("app.project.trackers[0].id")
    page.mouse.click(*at(1700, 150))
    pid = ev("app.selTracker")
    check("template + point created", ev("app.project.trackers.map(p => p.kind).join()") == "template,point")
    go()
    tkey = ev(f"app.project.segments(app.project.tracker({tid}))[0].key")
    check("tracked without bounds", ev(f"app.results.hi('{tkey}')") == 599)

    # ---- Push model: removing a tracker must not jump to the remaining mean --------
    push = ev(f"""(() => {{ const s = app.project.subjects[0];
        return {{ p100: app.project.subjectState(s, 100, app.results).x,
                  p101: app.project.subjectState(s, 101, app.results).x,
                  lone: app.project.trackerState(app.project.tracker({tid}), 101, app.results).x }}; }})()""")
    ev(f"app.edit('test: end the far point', () => app.project.endTrackerAt({pid}, 100))")
    after = ev("app.project.subjectState(app.project.subjects[0], 101, app.results).x")
    check("removing a tracker doesn't jump to the remaining mean",
          abs(after - push["p101"]) < 40 and abs(push["lone"] - after) > 200, (push, after))
    ev("app.undo()")

    # ---- Bounds mode: draw a box on frame 0 (no bounds yet -> whole video) ----------------
    sid = ev("app.selSubject")
    ev(f"app.select({{subject: {sid}, tracker: null}})")
    page.keyboard.press("b")
    check("B enters bounds mode", ev("app.viewer.mode.name") == "bounds")
    drag(at(380, 280), at(480, 380))
    ext = ev(f"app.project.boundsStore(app.project.subject({sid})).extent()")
    check("drawn box fills the whole video", ext == [0, 599], ext)
    b0 = ev(f"app.project.boundsAt(app.project.subject({sid}), 0)")
    check("box geometry", abs(b0[0] - 430) < 2 and abs(b0[2] - 100) < 3, b0)
    check("template results invalidated", ev(f"app.results.hi('{tkey}')") == 0)
    pkey = ev(f"app.project.segments(app.project.tracker({pid}))[0].key")
    check("CoTracker results kept", ev(f"app.results.hi('{pkey}')") == 599)
    page.keyboard.press("Escape")
    page.keyboard.press("Control+z")
    check("undo removes bounds and restores results",
          ev(f"app.project.boundsAt(app.project.subject({sid}), 0)") is None and ev(f"app.results.hi('{tkey}')") == 599)
    page.keyboard.press("Control+y")
    check("redo re-applies", ev(f"app.project.boundsAt(app.project.subject({sid}), 0) != null && app.results.hi('{tkey}') === 0"))
    page.keyboard.press("Control+z")

    # ---- Puppeteer pass from frame 10, following the true path with a human lag ----------
    seek(10)
    page.keyboard.press("p")
    check("P arms the puppeteer", ev("app.viewer.mode.name === 'puppeteer' && app.viewer.mode.state === 'armed'"))
    tip = lambda f: (TIPS[f][0] + 0.5, TIPS[f][1] + 0.5)  # noqa: E731
    page.mouse.click(*at(*tip(10)))
    page.wait_for_function("app.viewer.mode.state === 'recording'", timeout=5000)
    t_end = time.time() + 20
    live = None
    while time.time() < t_end:
        f = ev("app.shown")
        if f >= 80:
            break
        page.mouse.move(*at(*tip(max(10, f - 7))))
        if live is None and f >= 40:
            # The live box: it should sit under the hand (smoothed) and be at
            # least the minimum size, and it is the box the pass will keep.
            live = ev("""(() => { const m = app.viewer.puppeteer; const b = m.liveBox();
                const [x, y] = app.viewer.toSource(...m.mouse);
                return b && { cx: b[0], cy: b[1], w: b[2], h: b[3], mx: x, my: y }; })()""")
        time.sleep(0.012)
    check("live box follows the hand", live and abs(live["cx"] - live["mx"]) < 40 and abs(live["cy"] - live["my"]) < 40, live)
    check("live box has a real size", live and live["w"] >= 32 and live["h"] >= 32, live)
    page.keyboard.press("Escape")
    page.wait_for_function("app.viewer.mode.name === 'normal'", timeout=5000)
    passes = ev(f"app.project.subject({sid}).boundsPasses.map(p => ({{name: p.name, level: p.level, kind: p.kind, a: p.a, b: p.b}}))")
    ext = ev(f"app.project.boundsExtent(app.project.subject({sid}))")
    check("pass recorded bounds from frame 10", passes and passes[0]["a"] == 10 and 75 <= passes[0]["b"] <= 110, passes)
    check("effective bounds cover the pass", ext and ext[0] == 10 and 75 <= ext[1] <= 110, ext)
    check("playback rate restored", ev("app.video.playbackRate") == 1)
    # Disabling a pass reveals what is under it; undo restores it.
    ev(f"app.editBoundsPass('disable pass', {sid}, app.project.subject({sid}).boundsPasses[0].id, {{enabled: false}})")
    check("disabling the pass removes its bounds", ev(f"app.project.boundsAt(app.project.subject({sid}), 40)") is None)
    ev("app.undo()")
    check("undo restores the pass", ev(f"app.project.boundsAt(app.project.subject({sid}), 40)") is not None)
    inside = ev(f"""(() => {{ const s = app.project.subject({sid}); const tips = {json.dumps(TIPS[10:76])}; let n = 0;
        tips.forEach((t, i) => {{ const b = app.project.boundsAt(s, 10 + i); if (b && Math.abs(t[0] + .5 - b[0]) <= b[2] / 2 && Math.abs(t[1] + .5 - b[1]) <= b[3] / 2) n++; }});
        return n; }})()""")
    check("cursor inside the recorded box", inside >= 60, f"{inside}/66 frames")

    # ---- Drift: the far point is outside the bounds from frame 10 on ------------------------
    seek(40)
    st = ev(f"(() => {{ const s = app.project.subject({sid}); return app.project.subjectState(s, 40, app.results); }})()")
    check("far point flagged drifted", st["drifted"] == 1, st)
    check("drifted point not in the mean", st.get("n", 0) <= 1)
    check("not drifted before the bounds start", not ev(f"app.project.trackerState(app.project.tracker({pid}), 5, app.results).drifted"))
    ev(f"app.select({{subject: {sid}, tracker: null}})")
    btn = page.locator('[data-act="end-drifted"]')
    check("End drifted button shows the count", "(1)" in btn.inner_text(), btn.inner_text())
    btn.click()
    check("End drifted ends it where it left the bounds", ev(f"app.project.tracker({pid}).end") == 10)

    # ---- Template re-track inside the bounds ------------------------------------------------
    go()
    found = ev(f"[20, 40, 60].map(f => app.project.trackerState(app.project.tracker({tid}), f, app.results)).map(s => s && !s.lost && !s.drifted)")
    check("template found inside the recorded bounds", all(found), found)

    # ---- Falloff nudge in Bounds mode ---------------------------------------------------------
    seek(40)
    page.keyboard.press("b")
    R = ev("app.viewer.boundsMode.R")
    check("falloff defaults to 0.2 s", abs(R - 12) <= 1, R)
    before = ev(f"[40, 40 + Math.round({R} / 2), 40 + {R}].map(f => app.project.boundsAt(app.project.subject({sid}), f))")
    c = before[0]
    drag(at(c[0], c[1]), at(c[0] + 40, c[1]))
    after = ev(f"[40, 40 + Math.round({R} / 2), 40 + {R}].map(f => app.project.boundsAt(app.project.subject({sid}), f))")
    d = [a[0] - b[0] if a and b else None for a, b in zip(after, before)]
    check("nudge: full on the frame, half at R/2, none at R", abs(d[0] - 40) < 1.5 and abs(d[1] - 20) < 2 and (d[2] in (None, 0) or abs(d[2]) < 0.01), d)
    page.keyboard.press("Escape")

    # ---- Export ------------------------------------------------------------------------------
    data = ev("(async () => { const m = await import('/static/js/exporter.js'); return m.buildExport(app); })()")
    subj = data["subjects"][0]
    i40 = subj["track"]["frame"].index(40)
    check("export has bounds + drifted counts", subj["track"]["bounds"][i40] is not None and "driftedTrackers" in subj["track"])
    check("export tracker samples have drifted flags", "drifted" in subj["trackers"][0]["samples"])
    check("0 console errors", not errors, errors[:3])
    browser.close()

print(f"{sum(checks)}/{len(checks)} checks passed")
sys.exit(0 if all(checks) else 1)
