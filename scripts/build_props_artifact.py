#!/usr/bin/env python3
"""Build the two-first-person-views artifact with the take embedded.

The `assets` capability is not available on this account, so an artifact cannot
reference uploaded media — the video lives inside the page as a data URI,
against the 16 MB rendered-page cap (base64 inflates by 4/3).

Usage:
    python scripts/build_props_artifact.py
"""

from __future__ import annotations

import argparse
import base64
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TAKE = REPO / "recordings" / "obs" / "fpv_web.mp4"
CAP_BYTES = 16 * 1024 * 1024

HTML = r"""<title>Two Views, One Simulation</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Archivo:wght@500;600;700&family=Literata:opsz,wght@7..72,400;7..72,500&family=JetBrains+Mono:wght@400;500&display=swap">

<style>
  /* Light is the complete palette; the two dark blocks redefine only tokens,
     so the un-stamped (system) document resolves correctly either way. */
  :root {
    --ground:  #FAF8F3;
    --surface: #FFFFFF;
    --sunken:  #F1EFE7;
    --ink:     #1A1D16;
    --muted:   #6B7263;
    --line:    #E0DDD1;
    --accent:  #C2622F;   /* the player capsules */
    --accent-soft: #F3E3D7;
    --prop:    #2D62B8;   /* the rigid bodies */
    --prop-soft:   #DEE7F6;
    --good:    #5E7A31;
    --shadow:  0 1px 2px rgba(26,29,22,.05), 0 10px 30px rgba(26,29,22,.08);
  }
  @media (prefers-color-scheme: dark) {
    :root:not([data-theme="light"]) {
      --ground:  #12150F;
      --surface: #1B1F17;
      --sunken:  #171B13;
      --ink:     #EDEFE6;
      --muted:   #9BA391;
      --line:    #2E3427;
      --accent:  #E08A4F;
      --accent-soft: #3A2A1C;
      --prop:    #7BA3E8;
      --prop-soft:   #1D2739;
      --good:    #A3C264;
      --shadow:  0 1px 2px rgba(0,0,0,.4), 0 10px 30px rgba(0,0,0,.4);
    }
  }
  :root[data-theme="dark"] {
    --ground:  #12150F;
    --surface: #1B1F17;
    --sunken:  #171B13;
    --ink:     #EDEFE6;
    --muted:   #9BA391;
    --line:    #2E3427;
    --accent:  #E08A4F;
    --accent-soft: #3A2A1C;
    --prop:    #7BA3E8;
    --prop-soft:   #1D2739;
    --good:    #A3C264;
    --shadow:  0 1px 2px rgba(0,0,0,.4), 0 10px 30px rgba(0,0,0,.4);
  }

  * { box-sizing: border-box; }

  body {
    margin: 0;
    background: var(--ground);
    color: var(--ink);
    font-family: Literata, Georgia, serif;
    font-size: 17px;
    line-height: 1.65;
    -webkit-font-smoothing: antialiased;
  }

  .wrap { max-width: 1180px; margin: 0 auto; padding: 56px 24px 96px; }
  .col  { max-width: 68ch; }

  h1, h2, h3, .label { font-family: Archivo, "Helvetica Neue", Arial, sans-serif; }
  h1 {
    font-size: clamp(2.1rem, 5vw, 3.1rem);
    font-weight: 700;
    letter-spacing: -0.028em;
    line-height: 1.05;
    text-wrap: balance;
    margin: 0 0 18px;
  }
  h2 {
    font-size: 1.32rem; font-weight: 600; letter-spacing: -0.012em;
    text-wrap: balance; margin: 0 0 14px;
  }
  h3 { font-size: 1rem; font-weight: 600; margin: 0 0 6px; }
  p { margin: 0 0 16px; }
  a { color: var(--accent); text-underline-offset: 3px; }

  .label {
    font-size: .715rem; font-weight: 600; letter-spacing: .13em;
    text-transform: uppercase; color: var(--muted);
  }
  .lede { font-size: 1.16rem; color: var(--muted); margin-bottom: 30px; }
  .rule { height: 1px; background: var(--line); border: 0; margin: 52px 0; }
  section { margin-top: 52px; }
  .mono, code {
    font-family: "JetBrains Mono", ui-monospace, monospace;
    font-size: .86em; font-variant-numeric: tabular-nums;
  }

  /* --- the take --------------------------------------------------------- */
  .stage { margin: 0 0 14px; }
  .stage__frame {
    background: #000; border-radius: 10px; overflow: hidden;
    box-shadow: var(--shadow);
  }
  .stage__frame video { width: 100%; display: block; }
  .panes {
    display: grid; grid-template-columns: 1fr 1fr; gap: 0;
    margin-top: 10px;
  }
  .pane-tag {
    font-family: Archivo, sans-serif; font-weight: 600; font-size: .9rem;
    display: flex; align-items: center; gap: 9px;
  }
  .pane-tag:last-child { justify-content: flex-end; }
  .dot {
    width: 9px; height: 9px; border-radius: 50%;
    background: var(--accent); flex: none;
  }
  .stage__note { font-size: .92rem; color: var(--muted); margin-top: 12px; }

  /* --- measured numbers -------------------------------------------------- */
  .metrics {
    display: grid; grid-template-columns: repeat(auto-fit, minmax(210px, 1fr));
    gap: 1px; background: var(--line);
    border: 1px solid var(--line); border-radius: 10px; overflow: hidden;
    margin-top: 26px;
  }
  .metric { background: var(--surface); padding: 20px 22px; }
  .metric__value {
    font-family: Archivo, sans-serif; font-weight: 700;
    font-size: 1.85rem; letter-spacing: -0.02em;
    font-variant-numeric: tabular-nums; line-height: 1.1;
  }
  .metric__value.is-prop { color: var(--prop); }
  .metric__value.is-good { color: var(--good); }
  .metric__label {
    font-family: Archivo, sans-serif; font-size: .74rem; font-weight: 600;
    letter-spacing: .09em; text-transform: uppercase;
    color: var(--muted); margin-top: 8px;
  }
  .metric__note { font-size: .87rem; color: var(--muted); margin-top: 8px; }

  .facts { width: 100%; border-collapse: collapse; margin-top: 20px; font-size: .93rem; }
  .facts th, .facts td {
    text-align: left; padding: 11px 14px 11px 0;
    border-bottom: 1px solid var(--line); vertical-align: top;
  }
  .facts th {
    font-family: Archivo, sans-serif; font-weight: 600; font-size: .78rem;
    letter-spacing: .07em; text-transform: uppercase; color: var(--muted);
    white-space: nowrap; width: 38%;
  }

  .callout {
    background: var(--sunken); border-left: 3px solid var(--prop);
    border-radius: 0 8px 8px 0; padding: 18px 22px; margin-top: 22px;
  }
  .callout p:last-child { margin-bottom: 0; }

  footer {
    margin-top: 68px; padding-top: 22px; border-top: 1px solid var(--line);
    font-size: .88rem; color: var(--muted);
  }
  @media (prefers-reduced-motion: reduce) { * { transition: none !important; } }
</style>

<div class="wrap">

  <header class="col">
    <p class="label">new-soils · networked physics</p>
    <h1>Three hundred bodies, seen from both sides</h1>
    <p class="lede">
      Two players walk into a pile of 300 server-simulated rigid bodies, and
      into each other. Both first-person views are recorded into one frame, so
      the question the footage answers is whether they are looking at the same
      simulation.
    </p>
  </header>

  <hr class="rule">

  <section>
    <p class="label">The take</p>
    <h2>Left: alice. Right: bob. Same instant, same world.</h2>
    <figure class="stage">
      <div class="stage__frame">
        <video controls preload="metadata" muted playsinline loop
               src="data:video/mp4;base64,<!--VIDEO-->"></video>
      </div>
      <div class="panes">
        <span class="pane-tag"><span class="dot"></span>alice</span>
        <span class="pane-tag">bob<span class="dot"></span></span>
      </div>
      <figcaption class="stage__note">
        Not two recordings lined up afterwards — OBS composites the two client
        windows into a single 2560&times;720 canvas, so the two halves are the
        same frame by construction.
      </figcaption>
    </figure>
  </section>

  <section>
    <p class="label">Measured</p>
    <h2>What the tests found</h2>
    <div class="metrics">
      <div class="metric">
        <div class="metric__value is-prop">300</div>
        <div class="metric__label">bodies replicated</div>
        <p class="metric__note">Every one moving while the pile settles.</p>
      </div>
      <div class="metric">
        <div class="metric__value is-good">0.000</div>
        <div class="metric__label">worst disagreement</div>
        <p class="metric__note">
          Units, between the two clients' rest states. Not "close" — identical.
        </p>
      </div>
      <div class="metric">
        <div class="metric__value">621 B</div>
        <div class="metric__label">mean snapshot</div>
        <p class="metric__note">12.1 KB/s at the 20 Hz server tick.</p>
      </div>
    </div>
    <div class="col">
      <p style="margin-top:26px">
        The agreement figure is the one that matters. Each client receives delta
        snapshots over its own connection and rebuilds the world independently,
        so a client-side simulation quietly drifting into its own answer would
        show up here and essentially nowhere else.
      </p>
    </div>
  </section>

  <section class="col">
    <p class="label">Method</p>
    <h2>Why each client had to be a player</h2>
    <p>
      A first-person view has to come from that player's own camera and its own
      prediction. Pointing a spectator at someone else's replicated body would
      show their <em>interpolated, delayed</em> position — the opposite of what
      that player sees. So both clients are real players driven by a scripted
      input routine, with prediction, reconciliation and the local physics
      mirror all running exactly as they do for a person.
    </p>
    <div class="callout">
      <h3>A bug this surfaced</h3>
      <p>
        The first version of that routine assigned a whole input struct every
        frame. But <span class="mono">jump</span> and
        <span class="mono">toggle_fly</span> are <em>edge latches</em>, cleared
        only by the fixed tick that consumes them — and a rendered frame does
        not always contain a fixed tick. Overwriting the struct wiped latches
        before they were read, so one player silently never left fly mode and
        flew straight past the other. The keyboard path ORs latches in for
        precisely this reason.
      </p>
    </div>
  </section>

  <section class="col">
    <p class="label">Conditions</p>
    <h2>How the take was produced</h2>
    <table class="facts">
      <tbody>
        <tr><th>Bodies</th><td>300 cubes, dropped in a wide 3-layer lattice with deterministic jitter</td></tr>
        <tr><th>Solver</th><td>Avian, authoritative on the server, mirrored and predicted client-side</td></tr>
        <tr><th>Player contact</th><td>A kinematic proxy — one-way, so players shove props without props shoving back</td></tr>
        <tr><th>Capture</th><td>OBS window capture of both clients, 2560&times;720 at 60 fps</td></tr>
        <tr><th>Choreography</th><td>Both bots wait on one shared start file, so the two routines are released together</td></tr>
        <tr><th>Encode</th><td>121.5 MB source &rarr; 4.2 MB at 1600px, CRF 34</td></tr>
      </tbody>
    </table>
  </section>

  <footer class="col">
    Recorded from two release clients against an embedded release server, at
    noon with the day/night cycle pinned. Five tests back this scene:
    cross-client agreement, player-to-prop shoving, snapshot cost, the entity
    ceiling, and prop announcement.
  </footer>
</div>
"""


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=REPO / "artifact" / "props.html")
    args = ap.parse_args()

    if not TAKE.exists():
        sys.exit(f"missing take: {TAKE}\nRecord one with the props_demo test first.")
    data = base64.b64encode(TAKE.read_bytes()).decode("ascii")

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(HTML.replace("<!--VIDEO-->", data), encoding="utf-8")
    size = args.out.stat().st_size
    print(f"wrote {args.out} ({size / 1048576:.2f} MB of {CAP_BYTES / 1048576:.0f} MB cap)")
    if size > CAP_BYTES:
        sys.exit("over the artifact size cap — re-encode the take smaller")


if __name__ == "__main__":
    main()
