#!/usr/bin/env python3
"""Voiceless streaming overlay watcher.

Runs for the whole desktop session (systemd user service): follows
`voiceless ctl watch` and mirrors daemon sessions into an eww overlay
pill pinned bottom-center above everything (layer-shell, wm-ignore). The
pill opens when a session starts recording, unless the daemon turned the
overlay off for it (see "Gating"), and closes HIDE_AFTER_S after it ends;
nothing is spawned per hotkey press.

Silky path: instead of spawning `eww update` per event (~54 ms fork/exec),
the watcher writes one JSON line per frame into a FIFO that eww consumes via
deflisten, in-daemon. A 30 Hz loop eases the pill width toward its target
(exponential approach) so growth glides instead of stepping per character.

Text: a sliding window — once the transcript exceeds MAX_CHARS only the
newest MAX_CHARS characters are shown, flowing left as new words arrive.
State is border color only: live = blue (gold for the organize mode),
final = green, processing = gold.

Event flow. `voiceless ctl watch` subscribes to the daemon's `Event`
D-Bus signal (dev.voiceless.Daemon) and prints one JSON object per line:
the `stream --json` session events (`type` = ready, level, transcript,
finalized, warning, error, cancelled, completed; `transcript` carries
`authoritative_text` and `is_final`, `error` carries `message`) plus the
daemon's own `{"type": "state", "state": idle|recording|stopping,
"mode": ..., "overlay": true|false|null}` on every gateway transition and
`{"type": "log", ...}` lines. Each line may also name the `runtime_id` of
the daemon instance that published it (older daemons do not).
`watch_loop` parses them and `handle` maps them to the overlay:
`state: recording` opens the window and resets the pill, `transcript`
updates the live text (throttled) or marks it final, `finalized`,
`error` and `cancelled` paint the closing frame, `state: idle` schedules
the close. Unknown types are ignored.

Gating. One trigger opens the window: a `state: recording` event whose
`overlay` field is absent (a daemon too old to send it) or not false. Null
follows the desktop setting, and that setting is "show" wherever this
watcher is installed. `overlay: false` names a session the pill must not
mirror: a window left over from the previous session is closed, and that
session's transcript and terminal events are ignored until it is idle.
Events of a session whose start was never seen (the watcher connected
halfway) are ignored the same way.

Stale sessions. The pill must never outlive the session it mirrors, so it
closes at once instead of after HIDE_AFTER_S when that session's end
cannot arrive: a later event comes from another daemon instance (the
daemon restarted and the session died with it), `ctl watch` exits (bus
gone), or the watcher itself stops or starts again. The pill then stays
closed until the next session starts, because only the `state: recording`
event carries the `overlay` decision and the mode.

Threads: `watch_loop` (reader) and `open_fifo` (blocks until eww connects)
are daemon threads; `ease_loop` runs on the main thread. `handle`, `drop`
and the close check hold `session["lock"]`, which guards the session's
`pill` (LIVE, CLOSING or CLOSED); frames cross threads through the
`pending` queue, and `current_mode` is written only by the reader.

eww variables: every frame is one JSON line with `text`, `width`,
`pillbg` and `pillborder`; eww.yuck's `deflisten stream` turns it into
`${stream.*}` for the pill widget. Window open/close still goes through
`eww open|close voiceless-overlay`, the only per-session process spawns.

Environment: VOICELESS_BIN, EWW_BIN, EWW_CONFIG (set by the Home Manager
unit) and XDG_RUNTIME_DIR for the FIFO and the mode file; sizing comes
from voiceless-overlay.conf next to this script.
"""

import json
import os
import queue
import signal
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
CONF = os.path.join(HERE, "voiceless-overlay.conf")
EWW = os.environ.get("EWW_BIN", "eww")
EWW_CONFIG = os.environ.get("EWW_CONFIG", HERE)
VOICELESS = os.environ.get("VOICELESS_BIN", "voiceless")
RUNTIME = os.environ.get("XDG_RUNTIME_DIR", f"/tmp/voiceless-overlay-{os.getuid()}")
MODEFILE = os.path.join(RUNTIME, "voiceless-overlay.mode")
FIFO = os.path.join(RUNTIME, "voiceless-overlay.fifo")
WINDOW = "voiceless-overlay"

THROTTLE_S = 0.03          # min seconds between text frames
FRAME_S = 1.0 / 30.0       # width easing loop rate
EASE = 0.35                # per-frame approach factor toward target width
HIDE_AFTER_S = 2.0

# What the pill shows for the daemon session in progress. LIVE mirrors the
# session in an open window, CLOSING keeps its last frame up until
# `close_at`, CLOSED shows nothing: the overlay is off for the session or
# there is no session to mirror.
LIVE, CLOSING, CLOSED = "live", "closing", "closed"

MAX_WIDTH_PX = 720
MIN_WIDTH_PX = 170
CHAR_PX = 17
MAX_CHARS = 20

# (background, border) per state; state is shown by border color only.
STATE_COLORS = {
    "idle": ("rgba(36, 36, 40, 0.72)", "rgba(255, 255, 255, 0.14)"),
    "live": ("rgba(28, 37, 48, 0.78)", "rgba(98, 160, 234, 0.85)"),
    "final": ("rgba(36, 36, 40, 0.72)", "rgba(46, 194, 126, 0.8)"),
    "processing": ("rgba(28, 37, 48, 0.78)", "rgba(240, 201, 108, 0.9)"),
}
# Organize path uses gold so its (slower, LLM-backed) startup is visible.
ORGANIZE_LIVE = ("rgba(28, 37, 48, 0.78)", "rgba(240, 201, 108, 0.9)")
ORGANIZE_MODES = {"prompt-optimize"}
LISTENING = "聆听中…"

fifo = None
current_mode = "quick"
# (state, shown_text, target_width); the 30 Hz loop eases width toward it.
pending = queue.Queue()


def load_conf():
    """Override the four sizing integers from voiceless-overlay.conf.

    Format is `KEY=VALUE` with `#` comments; only MAX_WIDTH_PX,
    MIN_WIDTH_PX, CHAR_PX and MAX_CHARS are honoured. A missing file keeps
    the defaults above; a non-integer value raises and stops the service.
    """
    try:
        with open(CONF, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                key, _, value = line.partition("=")
                if key.strip() in ("MAX_WIDTH_PX", "MIN_WIDTH_PX", "CHAR_PX",
                                   "MAX_CHARS"):
                    globals()[key.strip()] = int(value.strip())
    except OSError:
        pass


def eww(*args):
    """Run one eww CLI command against the overlay's own config dir.

    Used only for `open` / `close` of the window; failures (eww daemon not
    up yet) are ignored, the next event simply tries again.
    """
    subprocess.run([EWW, "--config", EWW_CONFIG, *args], check=False,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def open_fifo():
    """Blocking open; unblocks when eww's deflisten reader connects."""
    global fifo
    if not os.path.exists(FIFO):
        os.mkfifo(FIFO)
    fifo = os.open(FIFO, os.O_WRONLY)


def frame(state, text, width):
    """Write one JSON frame (text, width, pillbg, pillborder) to the FIFO.

    `state` selects the colours: live is blue, or gold while the current
    mode is an organize mode; final green; processing gold; anything else
    the neutral idle pair. Frames are dropped silently until eww has
    opened the FIFO or if the write fails (reader gone); the ease loop
    keeps producing them, so the pill catches up on reconnect.
    """
    if fifo is None:
        return
    if state == "live" and current_mode in ORGANIZE_MODES:
        bg, border = ORGANIZE_LIVE
    else:
        bg, border = STATE_COLORS.get(state, STATE_COLORS["idle"])
    line = json.dumps({"text": text, "width": int(width), "pillbg": bg,
                       "pillborder": border}, ensure_ascii=False)
    try:
        os.write(fifo, (line + "\n").encode("utf-8"))
    except OSError:
        pass


def visible(text):
    """Sliding window: the newest MAX_CHARS characters of the transcript."""
    return text[-MAX_CHARS:] if len(text) > MAX_CHARS else text


def pill_width(text):
    """Target pill width in px for `text`, clamped to [MIN, MAX]_WIDTH_PX."""
    return max(MIN_WIDTH_PX, min(MAX_WIDTH_PX, MIN_WIDTH_PX + len(text) * CHAR_PX))


def drop(session):
    """Forget the session in progress and close its window at once.

    Caller holds `session["lock"]`. For a session that must not be mirrored
    or whose end will never be seen (see "Stale sessions" above), so unlike
    `state: idle` this leaves no closing frame up for HIDE_AFTER_S. The
    ease loop is reset to a neutral idle frame once the close returned, so
    nothing of the session is left to animate or to flash when the next
    window opens.
    """
    session.update(pill=CLOSED, close_at=None, runtime=None)
    eww("close", WINDOW)
    pending.put(("idle", "", MIN_WIDTH_PX))


def handle(event, session):
    """Translate one daemon event into pill frames and window commands.

    Runs on the reader thread under `session["lock"]`. `session` keeps
    `pill` (LIVE, CLOSING or CLOSED), `runtime` (`runtime_id` of the daemon
    instance whose session is on screen), `last_text` (last transcript
    shown; also the text painted on `finalized`), `last_frame` (monotonic
    time of the last live frame, for throttling) and `close_at` (when the
    ease loop should close the window, or None).
    """
    global current_mode
    kind = event.get("type")
    now = time.monotonic()
    runtime = event.get("runtime_id")
    owner = session["runtime"]
    if session["pill"] != CLOSED and runtime and owner and runtime != owner:
        # Another daemon instance published this event: the session on
        # screen ended with the previous one (restart, crash) and will
        # send nothing more.
        drop(session)
    if kind == "state":
        state = event.get("state")
        if state == "recording":
            if event.get("overlay") is False:
                # Overlay off for this session (see "Gating"): whatever is
                # on screen belongs to an earlier session and goes away.
                drop(session)
            else:
                # New session (no `overlay` field, or not false): remember
                # the mode (and persist it for the listener's fresh frame),
                # reset the per-session state, show the listening pill and
                # open the window. Any pending close from the previous
                # session is cancelled.
                current_mode = event.get("mode") or "quick"
                try:
                    with open(MODEFILE, "w") as f:
                        f.write(current_mode)
                except OSError:
                    pass
                session.update(last_text="", last_frame=0.0, close_at=None,
                               pill=LIVE, runtime=runtime)
                pending.put(("live", LISTENING, MIN_WIDTH_PX))
                eww("open", WINDOW)
        elif state == "idle" and session["pill"] == LIVE:
            # Session over (completed, cancelled or failed): keep the
            # closing frame visible for HIDE_AFTER_S, then close.
            session.update(pill=CLOSING, close_at=now + HIDE_AFTER_S)
        # "stopping" needs no frame: the last live text stays until final.
    elif session["pill"] == CLOSED:
        # No window: events of a session that opened none (overlay off, or
        # started before the watcher connected) change nothing on screen.
        pass
    elif kind == "transcript":
        snap = event.get("transcript") or {}
        text = snap.get("authoritative_text") or ""
        if snap.get("is_final"):
            # Final transcript: green pill with the text, or the gold
            # "processing" animation while an LLM mode still rewrites it.
            session["last_text"] = text
            if current_mode in ORGANIZE_MODES:
                pending.put(("processing", "整理中", pill_width("整理中···")))
            else:
                shown = visible(text or "No speech recognized")
                pending.put(("final", shown, pill_width(shown)))
        elif now - session["last_frame"] >= THROTTLE_S and text != session["last_text"]:
            # Live partial: at most one frame per THROTTLE_S and only when
            # the text changed, so a chatty backend cannot flood the FIFO.
            shown = visible(text or LISTENING)
            pending.put(("live", shown, pill_width(shown)))
            session.update(last_text=text, last_frame=now)
    elif kind == "finalized":
        # Injection done: repaint the final text (replaces the processing
        # animation for organize modes).
        shown = visible(session["last_text"] or "Done")
        pending.put(("final", shown, pill_width(shown)))
    elif kind == "error":
        msg = visible(event.get("message") or "Error")
        pending.put(("final", msg, pill_width(msg)))
    elif kind == "cancelled":
        pending.put(("final", "Cancelled", pill_width("Cancelled")))


def watch_loop(session):
    """Follow `voiceless ctl watch`, restarting it if the bus drops.

    Reader thread. Each stdout line is parsed as JSON and passed to
    `handle`; blank or malformed lines are skipped. When the child exits
    (bus gone; a daemon restart alone does not end `ctl watch`) the session
    on screen can no longer be followed, so `drop` closes it. The child is
    respawned after 1 s; if the binary cannot be started at all, after 2 s.
    """
    while True:
        try:
            child = subprocess.Popen([VOICELESS, "ctl", "watch"], stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, text=True)
        except OSError:
            time.sleep(2)
            continue
        for line in child.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            with session["lock"]:
                handle(event, session)
        child.wait()
        with session["lock"]:
            if session["pill"] != CLOSED:
                drop(session)
        time.sleep(1)


def ease_loop(session):
    """30 Hz render loop on the main thread.

    Drains `pending` to the newest (state, text, target width), eases the
    current width toward the target by EASE per frame (snapping once
    within 0.6 px), animates the "processing" dots every 0.4 s, writes a
    frame only when something changed, and closes the window once
    `session["close_at"]` has passed. A fresh listening frame jumps to its
    width instead of gliding from the previous session's; the idle frame
    that follows a `drop` settles the loop with the window closed.
    """
    state, text, target = "live", LISTENING, float(MIN_WIDTH_PX)
    width = float(MIN_WIDTH_PX)
    last = None
    proc_dots = 0
    proc_next = 0.0
    while True:
        try:
            while True:
                state, text, target = pending.get_nowait()
                target = float(target)
                if state == "live" and text == LISTENING:
                    width = target  # fresh session: no glide from the old width
                if state != "processing":
                    proc_dots = 0
        except queue.Empty:
            pass
        width += (target - width) * EASE
        if abs(target - width) < 0.6:
            width = target
        shown = text
        if state == "processing":
            now = time.monotonic()
            if now >= proc_next:
                proc_next = now + 0.4
                proc_dots = proc_dots % 3 + 1
            shown = "整理中" + "·" * proc_dots
        if (state, shown, width) != last:
            frame(state, shown, width)
            last = (state, shown, width)
        with session["lock"]:
            close_at = session["close_at"]
            if close_at is not None and time.monotonic() >= close_at:
                session.update(close_at=None, pill=CLOSED, runtime=None)
                eww("close", WINDOW)
        time.sleep(FRAME_S)


def main():
    """Load sizing, create the runtime dir, start the threads, render."""
    load_conf()
    os.makedirs(RUNTIME, exist_ok=True)
    session = {"lock": threading.Lock(), "last_text": "", "last_frame": 0.0,
               "close_at": None, "pill": CLOSED, "runtime": None}
    # A previous incarnation (service restart) may have left its window
    # open. Its session started before this one connected, so for this run
    # that pill is stale: close it before following events.
    eww("close", WINDOW)
    # systemd stops the service with SIGTERM. Leaving through `finally`
    # closes the window on screen, so it cannot outlive the watcher (see
    # "Stale sessions"); an error in the ease loop takes the same exit.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        # The FIFO writer stays open for the whole run; eww's deflisten
        # reader connects once when the eww daemon loads this config.
        threading.Thread(target=open_fifo, daemon=True).start()
        threading.Thread(target=watch_loop, args=(session,), daemon=True).start()
        ease_loop(session)
    finally:
        with session["lock"]:
            if session["pill"] != CLOSED:
                drop(session)


if __name__ == "__main__":
    main()
