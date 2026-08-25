#!/usr/bin/env python3
"""Build the public test-video dashboard and push it to the GCP VM.

The dashboard is a static page: every video listed in
`scripts/dashboard/videos.json` is re-encoded for the web, given a poster
frame, and uploaded alongside a generated `index.html`.

    python scripts/deploy_dashboard.py            # build + upload
    python scripts/deploy_dashboard.py --build    # build locally only

Requires ffmpeg on PATH and an authenticated gcloud.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "scripts/dashboard/videos.json"
OUT = ROOT / "target/dashboard"

PROJECT = "new-soils"
ZONE = "us-central1-a"
INSTANCE = "soils-dashboard"
HOST = "34.41.84.40"

# Anything wider than this is scaled down; the dashboard is watched in a
# browser column, not fullscreen, and the bitrate saving is most of the point.
MAX_WIDTH = 1280
CRF = 30
MIME = {".mp4": "video/mp4", ".webm": "video/webm"}


def gcloud() -> str:
    """gcloud is not always on PATH on Windows; fall back to the SDK location."""
    found = shutil.which("gcloud") or shutil.which("gcloud.cmd")
    if found:
        return found
    guess = Path(os.environ.get("LOCALAPPDATA", "")) / (
        "Google/Cloud SDK/google-cloud-sdk/bin/gcloud.cmd"
    )
    if guess.exists():
        return str(guess)
    sys.exit("gcloud not found; install the Cloud SDK or put it on PATH")


def run(cmd, **kw):
    print("$", " ".join(str(c) for c in cmd))
    return subprocess.run(cmd, check=True, **kw)


def probe(path: Path) -> dict:
    out = subprocess.run(
        [
            "ffprobe", "-v", "error", "-select_streams", "v:0",
            "-show_entries", "stream=width,height",
            "-show_entries", "format=duration", "-of", "json", str(path),
        ],
        check=True, capture_output=True, text=True,
    ).stdout
    d = json.loads(out)
    s = d["streams"][0]
    return {"w": s["width"], "h": s["height"], "dur": float(d["format"]["duration"])}


def encode(src: Path, dst: Path, info: dict) -> Path:
    """H.264 in MP4: every browser decodes it in hardware, which a 30 s clip on
    an e2-micro's uplink appreciates more than a marginally smaller VP9.

    Returns the file actually to be served. Some sources are already encoded
    harder than this pass would (the OBS clips are hand-tuned), and re-encoding
    them both inflates the file and stacks a second generation of loss — so the
    result is kept only when it is genuinely smaller.
    """
    scale = []
    if info["w"] > MAX_WIDTH:
        # -2 keeps the height even, which H.264 requires.
        scale = ["-vf", "scale=%d:-2" % MAX_WIDTH]
    run([
        "ffmpeg", "-y", "-loglevel", "error", "-i", str(src), *scale,
        "-c:v", "libx264", "-preset", "slow", "-crf", str(CRF),
        "-pix_fmt", "yuv420p", "-an", "-movflags", "+faststart", str(dst),
    ])
    if dst.stat().st_size < src.stat().st_size:
        return dst
    print("  source is already smaller; serving it as-is")
    kept = dst.with_suffix(src.suffix)
    dst.unlink()
    # Remux rather than copy, so an already-small source still gets its moov
    # atom at the front and starts playing before it has fully downloaded.
    run([
        "ffmpeg", "-y", "-loglevel", "error", "-i", str(src),
        "-c", "copy", "-movflags", "+faststart", str(kept),
    ])
    return kept


def poster(src: Path, dst: Path, info: dict):
    # A third of the way in: far enough past the fade-in to show the scene.
    at = max(0.5, info["dur"] / 3)
    scale = "scale=%d:-2" % min(MAX_WIDTH, info["w"])
    run([
        "ffmpeg", "-y", "-loglevel", "error", "-ss", str(at), "-i", str(src),
        "-frames:v", "1", "-vf", scale, "-q:v", "4", str(dst),
    ])


def human(n: int) -> str:
    return f"{n / 1e6:.1f} MB" if n >= 1e6 else f"{n / 1e3:.0f} kB"


def build() -> list:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    repo = manifest["repo"]
    ref = manifest.get("ref", "master")
    videos = OUT / "videos"
    videos.mkdir(parents=True, exist_ok=True)

    cards = []
    for v in manifest["videos"]:
        src = ROOT / v["source"]
        if not src.exists():
            print(f"  skip {v['id']}: {v['source']} is missing")
            continue
        info = probe(src)
        mp4, jpg = videos / f"{v['id']}.mp4", videos / f"{v['id']}.jpg"
        print(f"encoding {v['id']} ({info['w']}x{info['h']}, {info['dur']:.0f}s)")
        served = encode(src, mp4, info)
        poster(src, jpg, info)
        print(f"  {human(src.stat().st_size)} -> {human(served.stat().st_size)}")
        cards.append({
            **v,
            "mp4": f"videos/{served.name}",
            "mime": MIME.get(served.suffix, "video/mp4"),
            "jpg": f"videos/{jpg.name}",
            "bytes": served.stat().st_size,
            "dur": info["dur"],
            "w": info["w"],
            "h": info["h"],
        })

    cards.sort(key=lambda c: c["date"], reverse=True)
    (OUT / "index.html").write_text(render(cards, repo, ref), encoding="utf-8")
    print(f"built {OUT / 'index.html'} with {len(cards)} videos")
    return cards


CARD = """
    <article class="card">
      <video controls preload="none" poster="{jpg}" playsinline>
        <source src="{mp4}" type="{mime}">
      </video>
      <div class="meta">
        <h2>{title}</h2>
        <p class="sub"><time>{date}</time> &middot; {dur:.0f}s &middot; {size}
           &middot; {w}&times;{h}</p>
        <p class="blurb">{blurb}</p>
        <p class="tags">{tags}</p>
        <p class="src">produced by
          <a href="{repo}/blob/{ref}/{test}"><code>{test_name}</code></a>
          in <code>{file}</code></p>
      </div>
    </article>"""

PAGE = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>new-soils - test recordings</title>
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Crect width='16' height='16' fill='%230f1115'/%3E%3Crect x='2' y='7' width='12' height='6' fill='%237a5a3a'/%3E%3Crect x='2' y='6' width='12' height='2' fill='%237fd1a6'/%3E%3C/svg%3E">
<style>
  :root {{ color-scheme: dark; --bg:#0f1115; --card:#171a21; --line:#262b36;
           --fg:#e6e8ec; --dim:#9aa3b2; --accent:#7fd1a6; }}
  * {{ box-sizing: border-box; }}
  body {{ margin:0; padding:2rem 1rem 4rem; background:var(--bg); color:var(--fg);
          font:16px/1.6 ui-sans-serif,system-ui,"Segoe UI",Roboto,sans-serif; }}
  header {{ max-width:1100px; margin:0 auto 2.5rem; }}
  h1 {{ margin:0 0 .4rem; font-size:1.9rem; letter-spacing:-.02em; }}
  header p {{ margin:.3rem 0; color:var(--dim); max-width:62ch; }}
  a {{ color:var(--accent); }}
  main {{ max-width:1100px; margin:0 auto; display:grid; gap:1.5rem; }}
  .card {{ background:var(--card); border:1px solid var(--line); border-radius:12px;
           overflow:hidden; display:grid;
           grid-template-columns:minmax(0,1.15fr) minmax(0,1fr); }}
  .card video {{ width:100%; height:100%; max-height:420px; object-fit:contain;
                 background:#000; display:block; }}
  .meta {{ padding:1.25rem 1.4rem; min-width:0; }}
  .meta h2 {{ margin:0 0 .2rem; font-size:1.15rem; }}
  .sub {{ margin:0 0 .8rem; color:var(--dim); font-size:.85rem;
          font-variant-numeric:tabular-nums; }}
  .blurb {{ margin:0 0 .9rem; font-size:.93rem; }}
  .blurb code, .src code {{ background:#0b0d11; border:1px solid var(--line);
          border-radius:4px; padding:.05em .35em; font-size:.85em; }}
  .tags {{ margin:0 0 .7rem; display:flex; flex-wrap:wrap; gap:.4rem; }}
  .tags span {{ font-size:.72rem; letter-spacing:.04em; text-transform:uppercase;
          color:var(--dim); border:1px solid var(--line); border-radius:99px;
          padding:.15rem .55rem; }}
  .src {{ margin:0; font-size:.82rem; color:var(--dim); }}
  footer {{ max-width:1100px; margin:3rem auto 0; color:var(--dim); font-size:.85rem; }}
  @media (max-width: 820px) {{ .card {{ grid-template-columns:1fr; }} }}
</style>
</head>
<body>
<header>
  <h1>new-soils &mdash; test recordings</h1>
  <p>Screen recordings produced by the automated test suite of
     <a href="{repo}">new-soils</a>, a networked voxel sandbox with an
     authoritative Rust server. Every clip below is the output of a real test,
     not a hand-made demo &mdash; each names the test that produced it.</p>
  <p>Rebuilt by <code>scripts/deploy_dashboard.py</code>.</p>
</header>
<main>{items}
</main>
<footer>
  <p>Served from a Google Cloud e2-micro instance. Source and full docs at
     <a href="{repo}">{repo_short}</a>.</p>
</footer>
</body>
</html>
"""


def render(cards: list, repo: str, ref: str) -> str:
    items = "\n".join(
        CARD.format(
            jpg=c["jpg"], mp4=c["mp4"], mime=c["mime"],
            title=c["title"], date=c["date"],
            dur=c["dur"], size=human(c["bytes"]), w=c["w"], h=c["h"],
            blurb=c["blurb"],
            tags="".join(f"<span>{t}</span>" for t in c["tags"]),
            repo=repo, ref=ref, test=c["test"], test_name=c["test_name"],
            file=c["test"].split("/")[-1],
        )
        for c in cards
    )
    return PAGE.format(items=items, repo=repo, repo_short=repo.replace("https://", ""))


def upload():
    g = gcloud()
    # On Windows gcloud drives PuTTY, which stops on an uncached host key and
    # waits for a keypress no automated run will ever supply.
    base = [g, "compute", "--project", PROJECT]
    tail = ["--zone", ZONE, "--strict-host-key-checking=no"]
    run([*base, "ssh", INSTANCE, *tail, "--command", "rm -rf /tmp/dashboard"])
    # pscp --recurse copies the directory *into* the destination, so the
    # destination is the parent: /tmp/ yields /tmp/dashboard, not
    # /tmp/dashboard/dashboard.
    run([*base, "scp", *tail, "--recurse", str(OUT), f"{INSTANCE}:/tmp/"])
    run([*base, "ssh", INSTANCE, *tail, "--command",
         "sudo rm -rf /var/www/soils && sudo mv /tmp/dashboard /var/www/soils "
         "&& sudo chown -R www-data:www-data /var/www/soils "
         "&& sudo chmod -R a+rX /var/www/soils "
         "&& sudo systemctl reload nginx && echo deployed"])
    print(f"\nhttp://{HOST}/")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--build", action="store_true", help="build locally, do not upload")
    args = ap.parse_args()
    if not shutil.which("ffmpeg"):
        sys.exit("ffmpeg is required")
    build()
    if not args.build:
        upload()


if __name__ == "__main__":
    main()
