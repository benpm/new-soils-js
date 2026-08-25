#!/usr/bin/env python3
"""Drive OBS Studio to record the game window, via obs-websocket.

Why not `muesli/obs-cli`, which is the obvious tool for this: it is pinned to
obs-websocket **protocol 4** (`goobs v0.8.0`, still on master, default port
4444) and OBS 28+ ships protocol **5** only. Against OBS 32 it fails the
handshake outright:

    error: Failed auth: Client/server version mismatch? ...
    "obsWebSocketVersion":"5.7.3","rpcVersion":1

So this drives `obs-cmd` (github.com/grigio/obs-cmd), which speaks protocol 5
and has the same command shape (`recording start|stop|status`).

Usage:
    python scripts/obs_record.py ensure     # start OBS, load the capture scene
    python scripts/obs_record.py start
    python scripts/obs_record.py stop       # prints the file OBS wrote
    python scripts/obs_record.py status
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

OBS_EXE = Path(r"C:\Program Files\obs-studio\bin\64bit\obs64.exe")
WS_CONFIG = (
    Path(os.environ["APPDATA"]) / "obs-studio" / "plugin_config" / "obs-websocket" / "config.json"
)
COLLECTION = "soils-capture"
SCENE = "Soils"
REPO = Path(__file__).resolve().parent.parent
OUT_DIR = REPO / "recordings" / "obs"

# OBS force-killed (or crashed) leaves a modal "OBS Studio Crash Detected"
# dialog on the next start. `--disable-shutdown-check` does not suppress it on
# OBS 32, and the dialog blocks websocket startup entirely — so dismiss it.
DISMISS_CRASH_DIALOG = r'''
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$wc = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::NameProperty, "OBS Studio Crash Detected")
$dlg = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $wc)
if ($null -eq $dlg) { "no-dialog"; exit 0 }
$bc = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::NameProperty, "Run in Normal Mode")
$btn = $dlg.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $bc)
if ($null -eq $btn) { "no-button"; exit 1 }
$btn.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
"dismissed"
'''


def ws_url() -> str:
    cfg = json.loads(WS_CONFIG.read_text())
    if not cfg.get("server_enabled"):
        sys.exit(
            "OBS websocket server is disabled. Enable it in OBS "
            "(Tools > WebSocket Server Settings) or set server_enabled=true in "
            f"{WS_CONFIG}"
        )
    pw = cfg.get("server_password", "")
    return f"obsws://localhost:{cfg.get('server_port', 4455)}/{pw}"


def obs_cmd(*args: str, check: bool = True) -> str:
    exe = shutil.which("obs-cmd") or str(Path.home() / ".cargo" / "bin" / "obs-cmd.exe")
    result = subprocess.run(
        [exe, "--websocket", ws_url(), *args], capture_output=True, text=True
    )
    if check and result.returncode != 0:
        sys.exit(f"obs-cmd {' '.join(args)} failed:\n{result.stdout}{result.stderr}")
    return result.stdout.strip()


def obs_running() -> bool:
    out = subprocess.run(
        ["tasklist", "/FI", "IMAGENAME eq obs64.exe"], capture_output=True, text=True
    ).stdout
    return "obs64.exe" in out


def powershell(script: str) -> str:
    return subprocess.run(
        ["powershell", "-NoProfile", "-Command", script],
        capture_output=True,
        text=True,
    ).stdout.strip()


def canvas_size() -> tuple[int, int]:
    """Canvas the capture scene collection was generated for."""
    path = (
        Path(os.environ["APPDATA"]) / "obs-studio" / "basic" / "scenes" / f"{COLLECTION}.json"
    )
    try:
        res = json.loads(path.read_text(encoding="utf-8"))["resolution"]
        return int(res["x"]), int(res["y"])
    except Exception:
        return 1280, 720


def ensure() -> None:
    """Start OBS on the capture collection and configure the output."""
    if not obs_running():
        if not OBS_EXE.exists():
            sys.exit(f"OBS Studio not found at {OBS_EXE}")
        print("starting OBS…")
        subprocess.Popen(
            [str(OBS_EXE), "--collection", COLLECTION, "--scene", SCENE,
             "--disable-shutdown-check", "--minimize-to-tray"],
            cwd=str(OBS_EXE.parent),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        # The crash dialog appears within a few seconds of launch, before the
        # websocket server starts.
        for _ in range(20):
            time.sleep(1)
            if powershell(DISMISS_CRASH_DIALOG) == "dismissed":
                print("  dismissed the crash-recovery dialog")
                break

    for attempt in range(30):
        try:
            obs_cmd("recording", "status")
            break
        except SystemExit:
            if attempt == 29:
                raise
            time.sleep(2)

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    obs_cmd("scene-collection", "switch", COLLECTION, check=False)
    obs_cmd("record-directory", "set", str(OUT_DIR).replace("\\", "/"))
    # The canvas has to match the scene the collection actually defines — a
    # two-pane comparison is twice as wide, and a mismatched canvas silently
    # letterboxes or crops one pane away.
    w, h = canvas_size()
    obs_cmd(
        "video-settings", "set",
        "--base-width", str(w), "--base-height", str(h),
        "--output-width", str(w), "--output-height", str(h),
        "--fps-num", "60", "--fps-den", "1",
    )
    print(f"OBS ready: collection={COLLECTION} scene={SCENE} out={OUT_DIR}")


def newest_recording() -> Path | None:
    files = sorted(OUT_DIR.glob("*.mp4"), key=lambda p: p.stat().st_mtime)
    return files[-1] if files else None


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("action", choices=["ensure", "start", "stop", "status"])
    args = ap.parse_args()

    if args.action == "ensure":
        ensure()
    elif args.action == "start":
        print(obs_cmd("recording", "start"))
    elif args.action == "stop":
        print(obs_cmd("recording", "stop"))
        # OBS finalizes the file a moment after the request returns.
        time.sleep(3)
        f = newest_recording()
        print(f"recorded: {f}" if f else "no recording found")
    else:
        print(obs_cmd("recording", "status"))


if __name__ == "__main__":
    main()
