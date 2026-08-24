#!/usr/bin/env python3
"""Build the collision-demo artifact page with both takes embedded.

The `assets` capability is not available on this account, so an artifact cannot
reference uploaded media — the videos have to live inside the page as data
URIs, against the 16 MB rendered-page cap. Base64 inflates by 4/3, so the
encoder settings in `scripts/obs_record.py`'s sibling pipeline matter: keep the
two takes under ~5 MB combined.

Usage:
    python scripts/build_artifact.py [--out artifact/collision.html]
"""

from __future__ import annotations

import argparse
import base64
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TAKES = [
    {
        "id": "clean",
        "file": REPO / "recordings" / "obs" / "clean_web.mp4",
        "label": "Clean link",
        "spec": "0 ms · no jitter · 0% loss",
        "tone": "good",
        "note": "The baseline. Every input and snapshot arrives immediately.",
    },
    {
        "id": "laggy",
        "file": REPO / "recordings" / "obs" / "laggy_web.mp4",
        "label": "Degraded link",
        "spec": "120 ms ±40 ms · 5% loss",
        "tone": "accent",
        "note": "Gaussian jitter around a 120 ms one-way delay, losing 5% of "
        "the lanes built to absorb loss.",
    },
]

CAP_BYTES = 16 * 1024 * 1024


def encode(path: Path) -> str:
    if not path.exists():
        sys.exit(f"missing take: {path}\nRecord one with the demo test first.")
    return base64.b64encode(path.read_bytes()).decode("ascii")


def video_block(take: dict, data: str) -> str:
    return f"""
      <figure class="take take--{take['tone']}">
        <figcaption class="take__head">
          <span class="take__label">{take['label']}</span>
          <span class="take__spec">{take['spec']}</span>
        </figcaption>
        <div class="take__frame">
          <video id="v-{take['id']}" preload="metadata" muted playsinline loop
                 src="data:video/mp4;base64,{data}"></video>
        </div>
        <p class="take__note">{take['note']}</p>
      </figure>"""


def build() -> str:
    blocks = "\n".join(video_block(t, encode(t["file"])) for t in TAKES)
    return HTML.replace("<!--TAKES-->", blocks)


HTML = r"""<title>Collision Under Latency</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Archivo:wght@500;600;700&family=Literata:opsz,wght@7..72,400;7..72,500&family=JetBrains+Mono:wght@400;500&display=swap">

<style>
  /* Light palette is the complete set; the two dark blocks redefine only
     tokens, so an un-stamped (system) document still resolves either way. */
  :root {
    --ground:  #FAF8F3;
    --surface: #FFFFFF;
    --sunken:  #F1EFE7;
    --ink:     #1A1D16;
    --muted:   #6B7263;
    --line:    #E0DDD1;
    --accent:  #C2622F;
    --accent-soft: #F3E3D7;
    --good:    #5E7A31;
    --good-soft:   #E6EBD8;
    --shadow:  0 1px 2px rgba(26,29,22,.05), 0 8px 24px rgba(26,29,22,.06);
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
      --good:    #A3C264;
      --good-soft:   #242D18;
      --shadow:  0 1px 2px rgba(0,0,0,.4), 0 8px 24px rgba(0,0,0,.35);
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
    --good:    #A3C264;
    --good-soft:   #242D18;
    --shadow:  0 1px 2px rgba(0,0,0,.4), 0 8px 24px rgba(0,0,0,.35);
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
    font-size: 1.32rem;
    font-weight: 600;
    letter-spacing: -0.012em;
    text-wrap: balance;
    margin: 0 0 14px;
  }
  h3 {
    font-size: 1rem;
    font-weight: 600;
    letter-spacing: -0.004em;
    margin: 0 0 6px;
  }
  p { margin: 0 0 16px; }
  a { color: var(--accent); text-underline-offset: 3px; }

  .label {
    font-size: .715rem;
    font-weight: 600;
    letter-spacing: .13em;
    text-transform: uppercase;
    color: var(--muted);
  }

  .lede { font-size: 1.16rem; color: var(--muted); margin-bottom: 30px; }

  .rule { height: 1px; background: var(--line); border: 0; margin: 56px 0; }

  section { margin-top: 56px; }
  section:first-of-type { margin-top: 0; }

  /* --- comparison ------------------------------------------------------- */
  .transport {
    display: flex;
    align-items: center;
    gap: 14px;
    flex-wrap: wrap;
    margin: 0 0 20px;
  }
  button.sync {
    font-family: Archivo, sans-serif;
    font-size: .9rem;
    font-weight: 600;
    color: var(--ground);
    background: var(--ink);
    border: 0;
    border-radius: 7px;
    padding: 10px 20px;
    cursor: pointer;
    transition: opacity .15s ease;
  }
  button.sync:hover { opacity: .85; }
  button.sync:focus-visible { outline: 2px solid var(--accent); outline-offset: 3px; }
  .transport__hint { font-size: .9rem; color: var(--muted); }

  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(420px, 1fr));
    gap: 26px;
  }
  @media (max-width: 900px) { .grid { grid-template-columns: 1fr; } }

  .take { margin: 0; }
  .take__head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
    padding: 0 2px 10px;
    border-bottom: 2px solid var(--line);
    margin-bottom: 12px;
  }
  .take--good  .take__head { border-bottom-color: var(--good); }
  .take--accent .take__head { border-bottom-color: var(--accent); }
  .take__label {
    font-family: Archivo, sans-serif;
    font-weight: 600;
    font-size: 1.02rem;
  }
  .take__spec {
    font-family: "JetBrains Mono", ui-monospace, monospace;
    font-size: .78rem;
    font-variant-numeric: tabular-nums;
    padding: 3px 9px;
    border-radius: 5px;
  }
  .take--good  .take__spec { color: var(--good);   background: var(--good-soft); }
  .take--accent .take__spec { color: var(--accent); background: var(--accent-soft); }

  .take__frame {
    background: #000;
    border-radius: 10px;
    overflow: hidden;
    box-shadow: var(--shadow);
    aspect-ratio: 16 / 9;
  }
  .take__frame video { width: 100%; height: 100%; display: block; }
  .take__note { font-size: .92rem; color: var(--muted); margin: 12px 2px 0; }

  /* --- notes ------------------------------------------------------------ */
  .points { display: grid; gap: 22px; margin-top: 24px; }
  .point { display: grid; grid-template-columns: 26px 1fr; gap: 14px; }
  .point__mark {
    width: 10px; height: 10px; margin-top: 9px;
    border-radius: 50%;
    background: var(--accent);
  }
  .point p { margin: 0; color: var(--muted); font-size: .97rem; }

  .facts {
    width: 100%;
    border-collapse: collapse;
    margin-top: 20px;
    font-size: .93rem;
  }
  .facts th, .facts td {
    text-align: left;
    padding: 11px 14px 11px 0;
    border-bottom: 1px solid var(--line);
    vertical-align: top;
  }
  .facts th {
    font-family: Archivo, sans-serif;
    font-weight: 600;
    font-size: .78rem;
    letter-spacing: .07em;
    text-transform: uppercase;
    color: var(--muted);
    white-space: nowrap;
    width: 34%;
  }
  .facts td { color: var(--ink); }
  .facts code, .mono {
    font-family: "JetBrains Mono", ui-monospace, monospace;
    font-size: .86em;
    font-variant-numeric: tabular-nums;
  }

  .callout {
    background: var(--sunken);
    border-left: 3px solid var(--accent);
    border-radius: 0 8px 8px 0;
    padding: 18px 22px;
    margin-top: 22px;
  }
  .callout p:last-child { margin-bottom: 0; }
  .callout pre {
    font-family: "JetBrains Mono", ui-monospace, monospace;
    font-size: .8rem;
    line-height: 1.55;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 12px 14px;
    margin: 12px 0 0;
    overflow-x: auto;
  }

  footer {
    margin-top: 72px;
    padding-top: 22px;
    border-top: 1px solid var(--line);
    font-size: .88rem;
    color: var(--muted);
  }

  @media (prefers-reduced-motion: reduce) {
    * { animation: none !important; transition: none !important; }
  }
</style>

<div class="wrap">

  <header class="col">
    <p class="label">new-soils · networked physics</p>
    <h1>Two players, one body between them</h1>
    <p class="lede">
      Player-vs-player collision did not exist in this engine until now — bodies
      passed straight through one another. These takes show it working: one
      player is stopped by another, then stands and jumps on their head.
    </p>
  </header>

  <hr class="rule">

  <section>
    <p class="label">The comparison</p>
    <h2>The same routine, on two different links</h2>
    <div class="col">
      <p>
        Collision is resolved on the server, so a bad link cannot change the
        outcome — only how quickly the viewer learns about it. Play both and
        watch the motion, not the timing.
      </p>
    </div>

    <div class="transport">
      <button class="sync" id="toggle" type="button">Play both</button>
      <span class="transport__hint" id="hint">40 seconds · 1280×720 · 60 fps</span>
    </div>

    <div class="grid">
      <!--TAKES-->
    </div>
  </section>

  <section class="col">
    <p class="label">What to look for</p>
    <h2>Three things the footage should show</h2>
    <div class="points">
      <div class="point">
        <span class="point__mark"></span>
        <div>
          <h3>Contact, not overlap</h3>
          <p>
            The charging player stops roughly 0.6 units short — two bodies of
            0.3 half-width meeting face to face — and holds there. No sinking
            into each other, no bouncing off.
          </p>
        </div>
      </div>
      <div class="point">
        <span class="point__mark"></span>
        <div>
          <h3>Standing on a head</h3>
          <p>
            Being blocked on the way down is what sets <span class="mono">grounded</span>,
            so the upper player can jump from the lower one's head and land back
            on it. That is the whole feature in one gesture.
          </p>
        </div>
      </div>
      <div class="point">
        <span class="point__mark"></span>
        <div>
          <h3>Continuous remote motion</h3>
          <p>
            Remote bodies render two server ticks behind the newest snapshot.
            Under 5% loss the gaps between snapshots widen, but the interpolated
            path should stay smooth — no snap, no stutter, never backwards.
          </p>
        </div>
      </div>
    </div>
  </section>

  <section class="col">
    <p class="label">How it was recorded</p>
    <h2>obs-cli could not be used</h2>
    <p>
      <a href="https://github.com/muesli/obs-cli">muesli/obs-cli</a> is pinned to
      obs-websocket <strong>protocol 4</strong> — its master branch still
      requires <span class="mono">goobs v0.8.0</span> and defaults to port 4444.
      OBS 28 and later serve <strong>protocol 5</strong> only. Against OBS 32.1.2
      it fails the handshake outright:
    </p>
    <div class="callout">
      <pre>error: Failed auth: Client/server version mismatch?
  "obsWebSocketVersion":"5.7.3","rpcVersion":1</pre>
      <p style="margin-top:12px">
        These takes use <a href="https://github.com/grigio/obs-cmd">grigio/obs-cmd</a>
        instead, which speaks protocol 5 and keeps the same command shape
        (<span class="mono">recording start</span> / <span class="mono">stop</span>).
      </p>
    </div>
    <p style="margin-top:22px">
      OBS captures the client window directly, so the render loop is untouched.
      The client signals a ready-file once the world has actually streamed and
      meshed; that is the cue to start recording. A fixed delay opened earlier
      attempts on empty sky.
    </p>
  </section>

  <section class="col">
    <p class="label">Conditions</p>
    <h2>What the degraded link actually does</h2>
    <table class="facts">
      <tbody>
        <tr>
          <th>Latency</th>
          <td>120 ms mean, each way, applied to the spectating client</td>
        </tr>
        <tr>
          <th>Jitter</th>
          <td>Gaussian, 40 ms standard deviation, via Box–Muller</td>
        </tr>
        <tr>
          <th>Loss</th>
          <td>
            5%, on loss-tolerant lanes only — inputs re-send the last three
            frames, snapshots are delta-coded against acked baselines
          </td>
        </tr>
        <tr>
          <th>Ordering</th>
          <td>
            Preserved. The modelled transports are ordered streams, so
            reordering them would test a network that cannot occur
          </td>
        </tr>
        <tr>
          <th>Determinism</th>
          <td>Seeded, so a failing run reproduces exactly</td>
        </tr>
        <tr>
          <th>Backing tests</th>
          <td>
            Six concurrent tests, each client on its own OS thread, up to 100 at
            once; plus seven interpolation tests asserting smoothness under 33%
            and 50% snapshot loss
          </td>
        </tr>
      </tbody>
    </table>
  </section>

  <footer class="col">
    Recorded from the release client against an embedded release server, at noon
    with the day/night cycle pinned. Both takes run the identical scripted
    routine; only the spectator's link differs.
  </footer>
</div>

<script>
  (function () {
    var vids = Array.prototype.slice.call(document.querySelectorAll("video"));
    var btn = document.getElementById("toggle");
    var hint = document.getElementById("hint");
    var playing = false;

    function setLabel() {
      btn.textContent = playing ? "Pause both" : "Play both";
    }

    btn.addEventListener("click", function () {
      playing = !playing;
      if (playing) {
        // Restart together so the two routines stay comparable frame for frame.
        vids.forEach(function (v) { v.currentTime = 0; });
        vids.forEach(function (v) {
          var p = v.play();
          if (p && p.catch) {
            p.catch(function () {
              playing = false;
              setLabel();
              hint.textContent = "Playback was blocked — use each video's own controls.";
              vids.forEach(function (x) { x.controls = true; });
            });
          }
        });
      } else {
        vids.forEach(function (v) { v.pause(); });
      }
      setLabel();
    });

    // Individual controls stay available for frame-stepping.
    vids.forEach(function (v) {
      v.controls = true;
      v.addEventListener("ended", function () { playing = false; setLabel(); });
    });

    setLabel();
  })();
</script>
"""


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=REPO / "artifact" / "collision.html")
    args = ap.parse_args()

    html = build()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(html, encoding="utf-8")
    size = args.out.stat().st_size
    print(f"wrote {args.out} ({size / 1048576:.2f} MB of {CAP_BYTES / 1048576:.0f} MB cap)")
    if size > CAP_BYTES:
        sys.exit("over the artifact size cap — re-encode the takes smaller")


if __name__ == "__main__":
    main()
