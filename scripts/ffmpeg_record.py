#!/usr/bin/env python3
"""Record the game window with ffmpeg's X11 grabber — the CI counterpart to
`obs_record.py`.

Same command surface (`ensure` / `start` / `stop` / `status`), so the demo
tests drive either one through `SOILS_RECORDER` without knowing which.

Why a second recorder rather than running OBS on CI: `obs_record.py` is
Windows-only by construction — it reads the websocket password out of
`%APPDATA%` and launches `C:\\Program Files\\obs-studio\\bin\\64bit\\obs64.exe`.
OBS on a headless Linux runner would also need a compositor to capture from,
which is the thing an `x11grab` of the Xvfb display already is.

What this deliberately does *not* do is capture frames from inside the client.
`crates/soils-client/src/record.rs` explains why that approach was abandoned:
saving a PNG per frame perturbs the very frame clock the recording exists to
judge. Grabbing the X display leaves the render loop alone, exactly as OBS
does.

Environment:
    DISPLAY                 the X display to capture (required)
    SOILS_CAPTURE_SIZE      WxH of the region (default: the display's size)
    SOILS_CAPTURE_FPS       capture rate (default 30)
    SOILS_CAPTURE_TITLES    comma-separated window titles to tile left-to-right
                            before recording, for multi-client takes. Without a
                            window manager every window maps at the origin and
                            the last one mapped hides the rest, so they have to
                            be moved explicitly. Needs `xdotool`.
    SOILS_CAPTURE_DIR       output directory (default: <repo>/recordings/ci)
    SOILS_CAPTURE_SPEED     playback speed of the finished file (default 2.0,
                            1.0 disables). Halves the duration and drops every
                            other frame, so the published file is roughly half
                            the size.

Usage:
    python scripts/ffmpeg_record.py ensure
    python scripts/ffmpeg_record.py start
    python scripts/ffmpeg_record.py stop      # prints the file written
    python scripts/ffmpeg_record.py status
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUT_DIR = Path(os.environ.get("SOILS_CAPTURE_DIR", REPO / "recordings" / "ci"))
# A file rather than a variable because `start` and `stop` are separate
# processes — the test invokes this script once per action, the way it invokes
# obs-cmd against a long-running OBS.
STATE = OUT_DIR / ".recording.json"


def die(msg: str) -> None:
    sys.exit(f"ffmpeg_record: {msg}")


def display() -> str:
    d = os.environ.get("DISPLAY")
    if not d:
        die("DISPLAY is not set; start Xvfb and export DISPLAY first")
    return d


def ffmpeg() -> str:
    exe = shutil.which("ffmpeg")
    if not exe:
        die("ffmpeg is not on PATH")
    return exe


def display_size() -> tuple[int, int]:
    """The Xvfb screen dimensions, via xdpyinfo.

    x11grab silently records a black frame if the requested region runs past
    the screen, so the region is derived from the server rather than assumed.
    """
    explicit = os.environ.get("SOILS_CAPTURE_SIZE")
    if explicit:
        w, h = explicit.lower().split("x")
        return int(w), int(h)
    out = subprocess.run(
        ["xdpyinfo", "-display", display()], capture_output=True, text=True
    )
    if out.returncode != 0:
        die(f"xdpyinfo failed — is {display()} up?\n{out.stderr}")
    m = re.search(r"dimensions:\s+(\d+)x(\d+)", out.stdout)
    if not m:
        die("could not parse dimensions out of xdpyinfo")
    return int(m.group(1)), int(m.group(2))


def tile_windows(titles: list[str], screen_w: int, screen_h: int) -> None:
    """Lay the named windows out left-to-right across the screen.

    Without a window manager an X client's requested position is honoured
    verbatim and winit does not request one, so every window sits at the origin
    and only the topmost is visible. Moving them here is what makes a
    side-by-side two-client take possible at all; OBS does the equivalent with
    one scene pane per window.
    """
    if not shutil.which("xdotool"):
        die("SOILS_CAPTURE_TITLES needs xdotool, which is not on PATH")
    pane_w = screen_w // len(titles)
    for i, title in enumerate(titles):
        found = subprocess.run(
            ["xdotool", "search", "--name", re.escape(title)],
            capture_output=True,
            text=True,
        )
        ids = [w for w in found.stdout.split() if w]
        if not ids:
            die(f"no window titled {title!r} on {display()}")
        # Several X windows can carry the title (winit makes helper windows);
        # the last one listed is the mapped top-level in practice, and a wrong
        # pick shows up immediately as a black pane rather than silently.
        wid = ids[-1]
        subprocess.run(["xdotool", "windowmove", wid, str(i * pane_w), "0"], check=True)
        subprocess.run(
            ["xdotool", "windowsize", wid, str(pane_w), str(screen_h)], check=True
        )
        print(f"placed {title!r} (window {wid}) at {i * pane_w},0 {pane_w}x{screen_h}")
    # Give the server a moment to actually restack and redraw before the first
    # captured frame, or the take opens on the pre-move layout.
    time.sleep(1.0)


def speed_up(src: Path, fps: str, speed: float) -> Path:
    """Re-encode `src` at `speed`x, keeping the same frame rate.

    `setpts=PTS/speed` halves the presentation times, and pinning the output
    rate back to `fps` makes ffmpeg *drop* the frames that no longer fit rather
    than doubling the rate — so the result is half the duration and half the
    frames. Measured on a 10 s 24 fps clip: 240 frames to 122.

    Size falls by less than half, and by how much depends entirely on the
    content: dropping every other frame also makes the surviving ones less
    similar to their neighbours, so each costs more to encode. A high-entropy
    test pattern only shrank 21%; a bot walking a fixed script under a software
    rasteriser, which is mostly the same frame repeated, does much better.
    """
    dst = src.with_name(src.stem + "-x" + str(speed).replace(".", "_") + src.suffix)
    args = [
        ffmpeg(), "-hide_banner", "-loglevel", "warning", "-y",
        "-i", str(src),
        "-vf", f"setpts=PTS/{speed}",
        "-r", fps,
        "-an",
        "-c:v", "libx264", "-preset", "veryfast", "-crf", "23",
        "-pix_fmt", "yuv420p",
        str(dst),
    ]
    out = subprocess.run(args, capture_output=True, text=True)
    if out.returncode != 0 or not dst.exists() or dst.stat().st_size == 0:
        # Not fatal: a take at 1x is still a take. Say so and keep the original
        # rather than losing a recording to a post-process.
        msg = f"{speed}x pass failed, keeping the original"
        print(f"warning: {msg}\n{out.stderr}", file=sys.stderr)
        return src
    before, after = src.stat().st_size, dst.stat().st_size
    print(f"sped up {speed}x: {before // 1024} KiB -> {after // 1024} KiB")
    src.unlink(missing_ok=True)
    return dst


def read_state() -> dict | None:
    if not STATE.exists():
        return None
    try:
        return json.loads(STATE.read_text())
    except (json.JSONDecodeError, OSError):
        return None


def running(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except (OSError, ProcessLookupError):
        return False
    return True


def cmd_ensure() -> None:
    ffmpeg()
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    w, h = display_size()
    print(f"ready: ffmpeg on {display()} ({w}x{h}) -> {OUT_DIR}")


def cmd_status() -> None:
    ffmpeg()
    w, h = display_size()
    st = read_state()
    if st and running(st["pid"]):
        print(f"recording to {st['path']} (pid {st['pid']}) on {display()} {w}x{h}")
    else:
        print(f"idle on {display()} ({w}x{h})")


def cmd_start() -> None:
    st = read_state()
    if st and running(st["pid"]):
        die(f"already recording to {st['path']} (pid {st['pid']})")
    OUT_DIR.mkdir(parents=True, exist_ok=True)

    w, h = display_size()
    titles = [t for t in os.environ.get("SOILS_CAPTURE_TITLES", "").split(",") if t]
    if titles:
        tile_windows(titles, w, h)

    fps = os.environ.get("SOILS_CAPTURE_FPS", "30")
    out = OUT_DIR / f"take-{time.strftime('%Y%m%d-%H%M%S')}.mp4"
    log = out.with_suffix(".log")
    args = [
        ffmpeg(),
        "-hide_banner",
        "-loglevel", "warning",
        "-y",
        "-f", "x11grab",
        "-framerate", fps,
        "-video_size", f"{w}x{h}",
        "-i", f"{display()}+0,0",
        # veryfast keeps the encoder off the CPU the software renderer needs.
        # The runner has no GPU, so the client and ffmpeg share the same cores
        # and a slower preset costs frames in the thing being filmed.
        "-c:v", "libx264",
        "-preset", "veryfast",
        "-crf", "23",
        # Odd dimensions are rejected by yuv420p; the client window is even,
        # but the tiled pane width is a division and may not be.
        "-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2",
        "-pix_fmt", "yuv420p",
        str(out),
    ]
    handle = log.open("wb")
    proc = subprocess.Popen(
        args, stdin=subprocess.DEVNULL, stdout=handle, stderr=subprocess.STDOUT
    )
    # A grabber that cannot open the display exits immediately; catching that
    # here means the test fails at `start` rather than after a full take.
    time.sleep(1.5)
    if proc.poll() is not None:
        die(f"ffmpeg exited immediately ({proc.returncode}):\n{log.read_text()}")

    STATE.write_text(json.dumps({"pid": proc.pid, "path": str(out), "log": str(log)}))
    print(f"recording {out}")


def cmd_stop() -> None:
    st = read_state()
    if not st:
        die("no recording in progress")
    STATE.unlink(missing_ok=True)
    pid, path = st["pid"], Path(st["path"])
    if running(pid):
        # SIGINT, not SIGKILL: ffmpeg finalises the container on interrupt, and
        # a killed encode leaves an unplayable file with no moov atom.
        os.kill(pid, signal.SIGINT)
        for _ in range(100):
            if not running(pid):
                break
            time.sleep(0.1)
        else:
            os.kill(pid, signal.SIGKILL)
            print("warning: ffmpeg ignored SIGINT and was killed", file=sys.stderr)
    if not path.exists() or path.stat().st_size == 0:
        log = Path(st.get("log", ""))
        detail = log.read_text() if log.exists() else "(no log)"
        die(f"no video at {path}:\n{detail}")
    speed = float(os.environ.get("SOILS_CAPTURE_SPEED", "2.0"))
    if speed > 1.0:
        path = speed_up(path, os.environ.get("SOILS_CAPTURE_FPS", "30"), speed)

    # The `recorded: ` prefix is part of the contract, not decoration: the demo
    # tests parse it out of stdout (`strip_prefix("recorded: ")`) to find the
    # file they then assert on. Printing a bare path made `props_demo` fail
    # *after* it had successfully recorded a video.
    print(f"recorded: {path}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("action", choices=["ensure", "start", "stop", "status"])
    action = ap.parse_args().action
    {"ensure": cmd_ensure, "start": cmd_start, "stop": cmd_stop, "status": cmd_status}[
        action
    ]()


if __name__ == "__main__":
    main()
