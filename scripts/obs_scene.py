#!/usr/bin/env python3
"""Generate a dedicated OBS scene collection that captures the game window.

Writes `soils-capture.json` alongside the user's existing collections rather
than editing any of them: OBS scene collections are separate files, so this
leaves whatever is already set up untouched and reversible by deleting one
file.

The capture is a *window* capture, not a display capture. Display capture would
record the whole desktop — everything else the user has open — which is both a
worse recording and a privacy problem.

Usage:
    python scripts/obs_scene.py [--name soils-capture] [--width 1280] [--height 720]
"""

from __future__ import annotations

import argparse
import json
import os
import uuid
from pathlib import Path

# `title:class:exe`, the format OBS stores for a window capture.
WINDOW_CLASS = "Window Class"
WINDOW_EXE = "soils-client.exe"

# window_capture `method`: 0 automatic, 1 BitBlt, 2 Windows Graphics Capture.
# WGC captures a GPU-rendered window that does not have focus, which is the
# whole point here — the recording must not require stealing the desktop.
METHOD_WGC = 2
# window_capture `priority`: 0 title, 1 class, 2 executable. Two clients of the
# same executable can only be told apart by title, so panes match on title.
PRIORITY_TITLE = 0
PRIORITY_EXE = 2


def window_string(title: str) -> str:
    return f"{title}:{WINDOW_CLASS}:{WINDOW_EXE}"


def collections_dir() -> Path:
    return Path(os.environ["APPDATA"]) / "obs-studio" / "basic" / "scenes"


def pane(title: str, label: str, priority: int) -> dict:
    """One window-capture source."""
    return {
        "prev_ver": 536936450,
        "name": label,
        "uuid": str(uuid.uuid4()),
        "id": "window_capture",
        "versioned_id": "window_capture",
        "settings": {
            "window": window_string(title),
            "method": METHOD_WGC,
            "priority": priority,
            # No cursor: it is parked wherever the user left it and would sit
            # in the middle of the shot for the whole take.
            "cursor": False,
            "client_area": True,
            "compatibility": False,
        },
        "mixers": 0,
        "sync": 0,
        "flags": 0,
        "volume": 1.0,
        "balance": 0.5,
        "enabled": True,
        "muted": False,
        "push-to-mute": False,
        "push-to-mute-delay": 0,
        "push-to-talk": False,
        "push-to-talk-delay": 0,
        "hotkeys": {},
        "deinterlace_mode": 0,
        "deinterlace_field_order": 0,
        "monitoring_type": 0,
        "private_settings": {},
    }


def pane_item(
    source: dict, index: int, x: float, pane_w: int, canvas_w: int, canvas_h: int
) -> dict:
    # OBS stores a *relative* position alongside the absolute one and prefers
    # it. Relative space spans x in [-aspect, +aspect] and y in [-1, 1], so a
    # value copied from a 16:9 layout puts every pane in the same place on a
    # 32:9 canvas — which is how a two-pane comparison silently became one.
    aspect = canvas_w / canvas_h
    rel_x = 2.0 * x / canvas_h - aspect
    return {
        "name": source["name"],
        "source_uuid": source["uuid"],
        "visible": True,
        "locked": False,
        "rot": 0.0,
        "scale_ref": {"x": float(canvas_w), "y": float(canvas_h)},
        "align": 5,
        # No bounds: the game window's client area is exactly the canvas size,
        # so a 1:1 blit at the origin fills it. Bounds scaling here was a trap —
        # OBS stores bounds in a *relative* space whose width is 2*aspect, so a
        # "full width" value of 2.0 silently rendered at 0.5625 scale.
        "bounds_type": 0,
        "bounds_align": 0,
        "bounds_crop": False,
        "crop_left": 0,
        "crop_top": 0,
        "crop_right": 0,
        "crop_bottom": 0,
        "id": index + 1,
        "group_item_backup": False,
        "pos": {"x": x, "y": 0.0},
        "pos_rel": {"x": rel_x, "y": -1.0},
        "scale": {"x": 1.0, "y": 1.0},
        "scale_rel": {"x": 1.0, "y": 1.0},
        "bounds": {"x": 0.0, "y": 0.0},
        "bounds_rel": {"x": 0.0, "y": 0.0},
        "scale_filter": "disable",
        "blend_method": "default",
        "blend_type": "normal",
        "show_transition": {"duration": 0},
        "hide_transition": {"duration": 0},
        "private_settings": {},
    }


def build(name: str, width: int, height: int, panes: list[tuple[str, str]]) -> dict:
    """`panes` is [(window title, label)]; laid out left to right."""
    scene_name = "Soils"
    sources = [
        pane(title, label, PRIORITY_TITLE if len(panes) > 1 else PRIORITY_EXE)
        for title, label in panes
    ]
    pane_w = width // max(len(sources), 1)
    items = [
        pane_item(src, i, float(i * pane_w), pane_w, width, height)
        for i, src in enumerate(sources)
    ]

    scene = {
        "prev_ver": 536936450,
        "name": scene_name,
        "uuid": str(uuid.uuid4()),
        "id": "scene",
        "versioned_id": "scene",
        "settings": {
            "custom_size": False,
            "id_counter": len(items) + 1,
            "items": items,
        },
        "mixers": 0,
        "sync": 0,
        "flags": 0,
        "volume": 1.0,
        "balance": 0.5,
        "enabled": True,
        "muted": False,
        "push-to-mute": False,
        "push-to-mute-delay": 0,
        "push-to-talk": False,
        "push-to-talk-delay": 0,
        "hotkeys": {},
        "deinterlace_mode": 0,
        "deinterlace_field_order": 0,
        "monitoring_type": 0,
        "private_settings": {},
    }

    return {
        "current_scene": scene_name,
        "current_program_scene": scene_name,
        "scene_order": [{"name": scene_name}],
        "name": name,
        "sources": [scene, *sources],
        "groups": [],
        "quick_transitions": [],
        "transitions": [],
        # A cut, not a fade: a transition at the start of a take fades the
        # first frames in and muddies exactly the motion being judged.
        "current_transition": "Cut",
        "transition_duration": 0,
        "preview_locked": False,
        "scaling_enabled": False,
        "scaling_level": 0,
        "scaling_off_x": 0.0,
        "scaling_off_y": 0.0,
        "virtual-camera": {"type2": 3},
        "modules": {},
        "resolution": {"x": width, "y": height},
        "migration_resolution": {"x": width, "y": height},
        "version": 2,
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--name", default="soils-capture")
    ap.add_argument("--height", type=int, default=720)
    ap.add_argument(
        "--pane",
        action="append",
        metavar="TITLE",
        help="window title to capture; repeat for a side-by-side comparison",
    )
    args = ap.parse_args()

    titles = args.pane or ["new-soils (Rust/Bevy)"]
    # Label = whatever is in brackets, e.g. "new-soils [alice]" -> "alice".
    panes = [(t, t.partition("[")[2].rstrip("]").strip() or "Game") for t in titles]
    width = 1280 * len(panes)

    out = collections_dir() / f"{args.name}.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(build(args.name, width, args.height, panes), indent=4), encoding="utf-8")
    print(f"wrote {out}")
    for title, label in panes:
        print(f"  pane {label!r} <- {window_string(title)}")
    print(f"  canvas {width}x{args.height}")


if __name__ == "__main__":
    main()
