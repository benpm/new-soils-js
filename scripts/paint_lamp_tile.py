#!/usr/bin/env python3
"""Paint the Lamp Block's tile into `blocks.png`.

The atlas is a hand-authored 8x8 grid of 16x16 tiles and there is no tooling
for it, so a new block normally means opening an image editor and a binary
diff nobody can review. This script is the alternative: the tile is *code*,
so what changed is readable in the diff and the art can be regenerated or
tweaked without the original editor.

Idempotent — it paints one tile and leaves the other 63 byte-for-byte alone.

    python scripts/paint_lamp_tile.py            # paint tile 24
    python scripts/paint_lamp_tile.py --check    # verify without writing
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from PIL import Image

REPO = Path(__file__).resolve().parent.parent
ATLAS = REPO / "crates" / "soils-client" / "assets" / "blocks.png"
TILE = 16
COLS = 8
# First free tile: 0-23 are painted, 24-63 are the magenta "missing texture"
# checker. Must match `faces:` for Lamp Block in blocks.yaml.
LAMP_TILE = 24

# Warm amber, deliberately lighter than any existing tile so the lamp reads as
# the bright thing in a dark room even before its light is applied.
FRAME = (74, 52, 28, 255)
FRAME_HI = (104, 74, 40, 255)
GLASS = (255, 198, 92, 255)
GLASS_HI = (255, 236, 176, 255)
CORE = (255, 252, 226, 255)


def paint() -> Image.Image:
    """One 16x16 lamp face: a dark frame, a glowing pane, a hot centre."""
    img = Image.new("RGBA", (TILE, TILE))
    px = img.load()
    for y in range(TILE):
        for x in range(TILE):
            edge = min(x, y, TILE - 1 - x, TILE - 1 - y)
            if edge == 0:
                px[x, y] = FRAME
            elif edge == 1:
                px[x, y] = FRAME_HI
            else:
                # Radial falloff from the centre of the pane.
                dx, dy = x - 7.5, y - 7.5
                d = (dx * dx + dy * dy) ** 0.5
                if d < 2.2:
                    px[x, y] = CORE
                elif d < 4.4:
                    px[x, y] = GLASS_HI
                else:
                    px[x, y] = GLASS
    # Four frame studs, so the block does not read as a flat gradient at world
    # scale the way a pure radial does.
    for sx, sy in ((2, 2), (13, 2), (2, 13), (13, 13)):
        px[sx, sy] = FRAME
    return img


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--check", action="store_true", help="verify, do not write")
    args = ap.parse_args()

    atlas = Image.open(ATLAS).convert("RGBA")
    if atlas.size != (TILE * COLS, TILE * COLS):
        sys.exit(f"unexpected atlas size {atlas.size}, expected {(TILE*COLS, TILE*COLS)}")

    box = ((LAMP_TILE % COLS) * TILE, (LAMP_TILE // COLS) * TILE)
    tile = paint()

    if args.check:
        current = atlas.crop((box[0], box[1], box[0] + TILE, box[1] + TILE))
        if current.tobytes() == tile.tobytes():
            print(f"tile {LAMP_TILE} is up to date")
        else:
            sys.exit(f"tile {LAMP_TILE} differs from paint(); re-run without --check")
        return

    atlas.paste(tile, box)
    atlas.save(ATLAS)
    print(f"painted tile {LAMP_TILE} at {box[0]},{box[1]} in {ATLAS.relative_to(REPO)}")


if __name__ == "__main__":
    main()
