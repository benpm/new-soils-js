#!/usr/bin/env python3
"""Fold one branch's recorded videos into the GitHub Pages site.

The site is a `gh-pages` checkout laid out as:

    index.html          the shell: tabs, player, written fresh every run
    branches.json       the manifest the shell reads at load
    b/<slug>/*.mp4      one directory per branch

Tabs are per *branch*, and a Pages deploy replaces the whole site — so a run
on `feature-x` must not delete `master`'s videos. That is why this mutates an
existing checkout instead of publishing a freshly built directory: the other
branches' entries are data this run has to carry forward, not regenerate.

`index.html` is rewritten every run rather than merged, so a change to the
shell reaches the live site from whichever branch pushes next. Only
`branches.json` and `b/` accumulate.

Usage:
    python scripts/build_pages.py \
        --site <gh-pages checkout> \
        --branch <name> --commit <sha> \
        --videos <dir of .mp4 + .json captions> \
        [--live-branches <name> ...]
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
from datetime import datetime, timezone
from pathlib import Path

# Captions keyed by the recording's file stem. Kept here rather than in the
# workflow so the description of what a take *shows* lives beside the code that
# renders it, and an unlisted video still publishes (with its bare name).
CAPTIONS = {
    "inventory": (
        "The inventory loop",
        "Place a block, mine it, watch it drop, walk onto it to collect, and "
        "open the screen. Then the container loop: the crate comes off the "
        "hotbar, goes down, and is opened by right-clicking it. Recorded from "
        "<code>inventory_demo.rs</code>, which asserts every scripted beat "
        "fired — a take of nothing happening fails the test rather than "
        "publishing.",
    ),
    "props": (
        "Two clients, one prop pile",
        "Two bots walk mirrored routines into a pile of physics props and meet "
        "in the middle, each pane a real first-person view rather than a "
        "replicated body. Recorded from <code>props_demo.rs</code>.",
    ),
    "two-player": (
        "Two players meeting",
        "A third, spectating client watches two players collide, block each "
        "other, and stand on each other's head — a viewpoint neither "
        "participant has. Recorded from <code>demo.rs</code>.",
    ),
    "stdb": (
        "Lobby and chat",
        "The SpacetimeDB server browser and cross-client chat. Recorded from "
        "<code>stdb_demo.rs</code>.",
    ),
}


def slugify(branch: str) -> str:
    """A filesystem- and URL-safe directory name for a branch.

    Branch names may contain `/`, which would silently create nested
    directories and break the relative URLs in the manifest.
    """
    s = re.sub(r"[^A-Za-z0-9._-]+", "-", branch).strip("-")
    return s or "unnamed"


INDEX_HTML = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>soils — recorded test cases</title>
<style>
  :root {
    color-scheme: light dark;
    --bg: #14161a; --panel: #1c1f26; --line: #2b3039;
    --fg: #e6e8ec; --dim: #9aa3b2; --accent: #7dd3a0;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; background: var(--bg); color: var(--fg);
    font: 15px/1.55 ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
  }
  header { padding: 2rem 1.5rem 1rem; max-width: 1100px; margin: 0 auto; }
  h1 { margin: 0 0 .35rem; font-size: 1.5rem; letter-spacing: -.01em; }
  .sub { color: var(--dim); margin: 0; }
  .sub a { color: var(--accent); }
  main { max-width: 1100px; margin: 0 auto; padding: 0 1.5rem 4rem; }
  nav {
    display: flex; flex-wrap: wrap; gap: .4rem;
    border-bottom: 1px solid var(--line); padding-bottom: .75rem; margin: 1.5rem 0;
  }
  button.tab {
    font: inherit; cursor: pointer; color: var(--dim);
    background: transparent; border: 1px solid var(--line);
    border-radius: 999px; padding: .3rem .85rem;
  }
  button.tab:hover { color: var(--fg); }
  button.tab[aria-selected="true"] {
    color: #10131a; background: var(--accent); border-color: var(--accent);
  }
  .meta { color: var(--dim); font-size: .85rem; margin: 0 0 1.5rem; }
  .meta code { color: var(--fg); }
  figure {
    margin: 0 0 2.5rem; background: var(--panel);
    border: 1px solid var(--line); border-radius: 10px; overflow: hidden;
  }
  video { display: block; width: 100%; background: #000; }
  figcaption { padding: .9rem 1.1rem 1.1rem; }
  figcaption h2 { margin: 0 0 .35rem; font-size: 1.05rem; }
  figcaption p { margin: 0; color: var(--dim); }
  figcaption code { color: var(--fg); }
  .empty { color: var(--dim); padding: 2rem 0; }
</style>
</head>
<body>
<header>
  <h1>soils — recorded test cases</h1>
  <p class="sub">
    Videos rendered on CI by the <code>#[ignore]</code>d recording tests, one
    tab per branch. Source:
    <a href="https://github.com/benpm/new-soils-js">benpm/new-soils-js</a>.
  </p>
</header>
<main>
  <nav id="tabs"></nav>
  <div id="panel"><p class="empty">Loading…</p></div>
</main>
<script>
const $tabs = document.getElementById('tabs');
const $panel = document.getElementById('panel');

function esc(s) {
  return String(s).replace(/[&<>"]/g, c =>
    ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
}

function render(slug, data) {
  const b = data[slug];
  for (const el of $tabs.children)
    el.setAttribute('aria-selected', String(el.dataset.slug === slug));
  if (location.hash.slice(1) !== slug) history.replaceState(null, '', '#' + slug);

  if (!b || !b.videos.length) {
    $panel.innerHTML = '<p class="empty">No recordings for this branch.</p>';
    return;
  }
  const when = new Date(b.updated).toLocaleString();
  const parts = [
    `<p class="meta">Branch <code>${esc(b.branch)}</code> at ` +
    `<code>${esc(b.commit.slice(0, 7))}</code> — recorded ${esc(when)}</p>`
  ];
  for (const v of b.videos) {
    parts.push(
      `<figure>` +
      `<video controls preload="metadata" playsinline loop ` +
      `src="${esc(v.file)}"></video>` +
      `<figcaption><h2>${esc(v.title)}</h2><p>${v.description}</p></figcaption>` +
      `</figure>`
    );
  }
  $panel.innerHTML = parts.join('');
}

fetch('branches.json?' + Date.now())
  .then(r => r.ok ? r.json() : Promise.reject(r.status))
  .then(data => {
    // Default branch first, then most-recently-recorded, so the tab that
    // opens is the one a visitor almost always wants.
    const slugs = Object.keys(data).sort((a, b) => {
      if ((data[a].branch === 'master') !== (data[b].branch === 'master'))
        return data[a].branch === 'master' ? -1 : 1;
      return data[b].updated.localeCompare(data[a].updated);
    });
    if (!slugs.length) {
      $panel.innerHTML = '<p class="empty">Nothing recorded yet.</p>';
      return;
    }
    for (const slug of slugs) {
      const btn = document.createElement('button');
      btn.className = 'tab';
      btn.dataset.slug = slug;
      btn.textContent = data[slug].branch;
      btn.onclick = () => render(slug, data);
      $tabs.append(btn);
    }
    const want = decodeURIComponent(location.hash.slice(1));
    render(slugs.includes(want) ? want : slugs[0], data);
  })
  .catch(e => {
    $panel.innerHTML = '<p class="empty">Could not load branches.json (' +
      esc(e) + ').</p>';
  });
</script>
</body>
</html>
"""


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--site", required=True, type=Path, help="gh-pages checkout")
    ap.add_argument("--branch", required=True)
    ap.add_argument("--commit", required=True)
    ap.add_argument("--videos", required=True, type=Path)
    ap.add_argument(
        "--live-branches",
        nargs="*",
        default=None,
        help="branches that still exist on the remote; entries for any others "
        "are pruned. Omit to prune nothing.",
    )
    args = ap.parse_args()

    site: Path = args.site
    site.mkdir(parents=True, exist_ok=True)
    manifest_path = site / "branches.json"
    manifest = {}
    if manifest_path.exists():
        try:
            manifest = json.loads(manifest_path.read_text())
        except json.JSONDecodeError:
            print("warning: branches.json was unreadable; starting a fresh one")

    slug = slugify(args.branch)
    dest = site / "b" / slug
    # Replace rather than merge: a take that stopped being recorded should
    # disappear from the tab, not linger as a stale entry the manifest no
    # longer lists.
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)

    videos = []
    for mp4 in sorted(args.videos.glob("*.mp4")):
        if mp4.stat().st_size == 0:
            print(f"skipping empty {mp4.name}")
            continue
        shutil.copy2(mp4, dest / mp4.name)
        title, description = CAPTIONS.get(mp4.stem, (mp4.stem, ""))
        videos.append(
            {
                "name": mp4.stem,
                "file": f"b/{slug}/{mp4.name}",
                "title": title,
                "description": description,
                "bytes": mp4.stat().st_size,
            }
        )
        print(f"published {mp4.name} ({mp4.stat().st_size // 1024} KiB)")

    if not videos:
        # An entry with no videos would render an empty tab, which reads as a
        # broken site rather than as "this run recorded nothing".
        print("no videos to publish; leaving the manifest entry off")
        manifest.pop(slug, None)
        shutil.rmtree(dest, ignore_errors=True)
    else:
        manifest[slug] = {
            "branch": args.branch,
            "commit": args.commit,
            "updated": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "videos": videos,
        }

    # An empty list means the remote listing failed, not that every branch was
    # deleted — pruning on it would wipe the whole site.
    if args.live_branches:
        live = {slugify(b) for b in args.live_branches} | {slug}
        for gone in [s for s in manifest if s not in live]:
            print(f"pruning deleted branch {manifest[gone]['branch']!r}")
            del manifest[gone]
            shutil.rmtree(site / "b" / gone, ignore_errors=True)

    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    (site / "index.html").write_text(INDEX_HTML)
    # Jekyll is on by default for Pages and skips paths beginning with an
    # underscore; nothing here does, but the build step is pure latency.
    (site / ".nojekyll").write_text("")
    print(f"site now lists {len(manifest)} branch(es)")


if __name__ == "__main__":
    main()
