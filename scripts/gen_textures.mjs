#!/usr/bin/env node
// Block textures: SVG-authored, rendered to PNG.
//
//   node scripts/gen_textures.mjs            # regenerate everything
//   node scripts/gen_textures.mjs --only 2,3 # subset (contact sheet still full)
//
// Every atlas tile index (see crates/soils-client/assets/blocks.yaml) gets a
// 1024×1024 "mega-tile" that repeats over 16×16 blocks (64 px per block). The
// client samples them as one texture array: `assets/blocks_mega.png` is the
// 24 tiles stacked vertically (1024 × 24576) and reinterpreted as 24 layers at
// load (gpu_mesh.rs). Tile row 0 of each 64 px band is the TOP of a block on
// side faces (same convention as the old 16 px atlas).
//
// Outputs:
//   scripts/textures/svg/<idx>_<name>.svg   editable vector sources
//   scripts/textures/contact_sheet.png      all tiles at 256 px, for review
//   crates/soils-client/assets/blocks_mega.png
//
// Style: cartoonish — 2–4 flat colours per tile, soft rounded shapes, thin
// darker outlines on discrete features (bricks, cobbles, nuggets). Everything
// is opaque (the terrain pipeline does no blending). Field tiles are seamless
// by construction (shapes crossing an edge get ±1024 wrapped copies); cell
// tiles keep each feature inside its own 64 px cell.

import { mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { Resvg } = require("@resvg/resvg-js");
const { PNG } = require("pngjs");

const SIZE = 1024;
const CELL = 64;
// The terrain shader lifts albedo into a physical-light exposure regime tuned
// for the old, darker 16 px atlas (dirt ≈ #5a4320). The SVG palettes are
// authored at a comfortable screen brightness and scaled down here at export
// so the game does not read over-exposed. SVG sources keep the authored hex.
const ALBEDO_SCALE = 0.66;
const CELLS = SIZE / CELL;
const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const SVG_DIR = join(ROOT, "scripts", "textures", "svg");
const ASSETS = join(ROOT, "crates", "soils-client", "assets");

// ---------------------------------------------------------------- helpers

function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const f = (v) => (Math.round(v * 10) / 10).toString();

/** Soft closed blob around (cx, cy): Catmull-Rom loop → cubic path data. */
function blobPath(rng, cx, cy, r, opts = {}) {
  const { jitter = 0.3, elong = 1 + rng() * 0.5, points = 6 + Math.floor(rng() * 4) } = opts;
  const angle = rng() * Math.PI * 2;
  const phase = rng() * Math.PI * 2;
  const pts = [];
  for (let i = 0; i < points; i++) {
    const a = phase + (i / points) * Math.PI * 2 + (rng() - 0.5) * 0.3;
    const rr = r * (1 + (rng() * 2 - 1) * jitter);
    const lx = Math.cos(a) * rr * elong, ly = Math.sin(a) * rr;
    pts.push([cx + lx * Math.cos(angle) - ly * Math.sin(angle), cy + lx * Math.sin(angle) + ly * Math.cos(angle)]);
  }
  const n = pts.length;
  let d = `M${f(pts[0][0])} ${f(pts[0][1])}`;
  for (let i = 0; i < n; i++) {
    const p0 = pts[(i - 1 + n) % n], p1 = pts[i], p2 = pts[(i + 1) % n], p3 = pts[(i + 2) % n];
    const c1 = [p1[0] + (p2[0] - p0[0]) / 6, p1[1] + (p2[1] - p0[1]) / 6];
    const c2 = [p2[0] - (p3[0] - p1[0]) / 6, p2[1] - (p3[1] - p1[1]) / 6];
    d += `C${f(c1[0])} ${f(c1[1])} ${f(c2[0])} ${f(c2[1])} ${f(p2[0])} ${f(p2[1])}`;
  }
  return d + "Z";
}

/** Emit `inner` plus wrapped copies for every ±SIZE offset whose shifted bbox touches the tile. */
function wrapped(inner, bbox) {
  const [x0, y0, x1, y1] = bbox;
  let out = "";
  for (const dx of [-SIZE, 0, SIZE]) for (const dy of [-SIZE, 0, SIZE]) {
    if (x1 + dx < 0 || x0 + dx > SIZE || y1 + dy < 0 || y0 + dy > SIZE) continue;
    out += dx || dy ? `<g transform="translate(${dx} ${dy})">${inner}</g>\n` : inner + "\n";
  }
  return out;
}

/** Scatter `count` blobs over the tile, toroidally spaced, wrapped at edges. */
function blobField(rng, { count, rMin, rMax, fill, stroke, jitter, spacing = 1.0, extra = "" }) {
  const centres = [];
  let out = `<g fill="${fill}"${stroke ? ` stroke="${stroke}" stroke-width="3" stroke-linejoin="round"` : ""}${extra}>\n`;
  let guard = 0;
  while (centres.length < count && guard++ < 30000) {
    const cx = rng() * SIZE, cy = rng() * SIZE, r = rMin + rng() * (rMax - rMin);
    const close = centres.some(([x, y, pr]) => {
      const dx = Math.min(Math.abs(x - cx), SIZE - Math.abs(x - cx));
      const dy = Math.min(Math.abs(y - cy), SIZE - Math.abs(y - cy));
      return Math.hypot(dx, dy) < (pr + r) * spacing;
    });
    if (close) continue;
    centres.push([cx, cy, r]);
    const d = blobPath(rng, cx, cy, r, { jitter });
    const R = r * 2.2;
    out += wrapped(`<path d="${d}"/>`, [cx - R, cy - R, cx + R, cy + R]);
  }
  return out + "</g>\n";
}

/** Small dots scattered over the tile (pebbles, speckles), wrapped. */
function dots(rng, { count, rMin, rMax, fill }) {
  let out = `<g fill="${fill}">\n`;
  for (let i = 0; i < count; i++) {
    const cx = rng() * SIZE, cy = rng() * SIZE, r = rMin + rng() * (rMax - rMin);
    out += wrapped(`<circle cx="${f(cx)}" cy="${f(cy)}" r="${f(r)}"/>`, [cx - r, cy - r, cx + r, cy + r]);
  }
  return out + "</g>\n";
}

/** Periodic wavy horizontal edge y = base + Σ sin: seamless in x by construction. */
function wave(x, base, amp, rng, terms) {
  let y = base;
  for (const [k, ph, w] of terms) y += Math.sin((x / SIZE) * Math.PI * 2 * k + ph) * amp * w;
  return y;
}
function waveTerms(rng, n = 3) {
  return Array.from({ length: n }, (_, i) => [2 + i * 3 + Math.floor(rng() * 3), rng() * Math.PI * 2, 1 / (i + 1)]);
}

/** Call `fn(x0, y0, rng, ix, iy)` for every 64 px cell; concatenate the SVG it returns. */
function cells(rng, fn) {
  let out = "";
  for (let iy = 0; iy < CELLS; iy++) for (let ix = 0; ix < CELLS; ix++) out += fn(ix * CELL, iy * CELL, rng, ix, iy);
  return out;
}

const rect = (fill) => `<rect width="${SIZE}" height="${SIZE}" fill="${fill}"/>\n`;
const pick = (rng, arr) => arr[Math.floor(rng() * arr.length)];

// ---------------------------------------------------------------- layers reused by several tiles

function dirtLayers(rng, p) {
  return (
    rect(p.base) +
    blobField(rng, { count: 26, rMin: 34, rMax: 70, fill: p.shadow, jitter: 0.35 }) +
    blobField(rng, { count: 18, rMin: 18, rMax: 40, fill: p.light, jitter: 0.3 }) +
    dots(rng, { count: 90, rMin: 2.5, rMax: 5, fill: p.shadow }) +
    dots(rng, { count: 40, rMin: 2, rMax: 4, fill: p.light })
  );
}
const DIRT = { base: "#9A6B43", shadow: "#7E5433", light: "#B58455" };
const TOUGH = { base: "#7E5533", shadow: "#63421F", light: "#96683F" };

function stoneLayers(rng, p = STONE) {
  return (
    rect(p.base) +
    blobField(rng, { count: 24, rMin: 36, rMax: 80, fill: p.shadow, jitter: 0.3 }) +
    blobField(rng, { count: 22, rMin: 24, rMax: 56, fill: p.light, jitter: 0.3 }) +
    cracks(rng, 14, p.crack)
  );
}
const STONE = { base: "#9C9FA3", shadow: "#85888D", light: "#B0B3B7", crack: "#6F7276" };

function cracks(rng, count, stroke) {
  let out = `<g fill="none" stroke="${stroke}" stroke-width="3" stroke-linecap="round" stroke-linejoin="round">\n`;
  for (let i = 0; i < count; i++) {
    let x = rng() * SIZE, y = rng() * SIZE;
    let d = `M${f(x)} ${f(y)}`;
    const n = 2 + Math.floor(rng() * 3), bb = [x, y, x, y];
    for (let k = 0; k < n; k++) {
      x += (rng() - 0.5) * 60; y += (rng() - 0.5) * 60;
      d += `L${f(x)} ${f(y)}`;
      bb[0] = Math.min(bb[0], x); bb[1] = Math.min(bb[1], y); bb[2] = Math.max(bb[2], x); bb[3] = Math.max(bb[3], y);
    }
    out += wrapped(`<path d="${d}"/>`, bb);
  }
  return out + "</g>\n";
}

const GRASS = { base: "#7DBE5A", light: "#93CF6C", shadow: "#5FA347", blade: "#6BB04B" };

function grassTop(rng) {
  return (
    rect(GRASS.base) +
    blobField(rng, { count: 22, rMin: 40, rMax: 80, fill: GRASS.shadow, jitter: 0.3, spacing: 1.1 }) +
    blobField(rng, { count: 26, rMin: 38, rMax: 72, fill: GRASS.light, jitter: 0.3, spacing: 1.1 })
  );
}

/** Grass cap + hanging blades at the top of every 64 px band (drawn over dirt). */
function grassCap(rng) {
  let out = `<g id="grass-cap">\n`;
  for (let band = 0; band < CELLS; band++) {
    const y0 = band * CELL;
    const terms = waveTerms(rng, 3);
    // Scalloped bottom edge: base depth 13 px, ±4 px periodic wobble.
    let d = `M0 ${y0 - 1}`;
    for (let x = 0; x <= SIZE; x += 8) d += `L${x} ${f(wave(x, y0 + 13, 4, rng, terms))}`;
    d += `L${SIZE} ${y0 - 1}Z`;
    out += `<path d="${d}" fill="${GRASS.base}"/>\n`;
    // Lighter top fringe.
    out += `<rect x="0" y="${y0}" width="${SIZE}" height="4" fill="${GRASS.light}"/>\n`;
    // Blades: tapered, hanging from the cap, drawn 3× for horizontal wrap.
    let blades = "";
    for (let x = rng() * 6; x < SIZE; x += 7 + rng() * 9) {
      const top = wave(x, y0 + 11, 4, rng, terms), len = 7 + rng() * 12, w = 3 + rng() * 3, lean = (rng() - 0.5) * 6;
      blades += `<path d="M${f(x - w)} ${f(top)}Q${f(x + lean)} ${f(top + len * 0.6)} ${f(x + lean * 1.5)} ${f(top + len)}Q${f(x + lean)} ${f(top + len * 0.6)} ${f(x + w)} ${f(top)}Z"/>`;
    }
    out += `<g fill="${GRASS.blade}">${blades}<g transform="translate(-${SIZE} 0)">${blades}</g><g transform="translate(${SIZE} 0)">${blades}</g></g>\n`;
  }
  return out + "</g>\n";
}

// ---------------------------------------------------------------- cell-feature tiles

function cobbles(rng, p) {
  // Irregular packed stones over mortar: toroidally spaced so the field is
  // seamless, sizes mixed so it does not read as a dot grid at distance.
  let out = rect(p.mortar);
  const centres = [];
  let guard = 0;
  let stones = "";
  while (centres.length < 900 && guard++ < 60000) {
    const cx = rng() * SIZE, cy = rng() * SIZE, r = 9 + rng() * rng() * 22;
    const close = centres.some(([x, y, pr]) => {
      const dx = Math.min(Math.abs(x - cx), SIZE - Math.abs(x - cx));
      const dy = Math.min(Math.abs(y - cy), SIZE - Math.abs(y - cy));
      return Math.hypot(dx, dy) < (pr + r) * 1.08 + 2;
    });
    if (close) continue;
    centres.push([cx, cy, r]);
    const d = blobPath(rng, cx, cy, r, { jitter: 0.14, elong: 1 + rng() * 0.35, points: 7 });
    const R = r * 1.8;
    stones += wrapped(`<path d="${d}" fill="${pick(rng, p.stones)}"/>`, [cx - R, cy - R, cx + R, cy + R]);
  }
  out += `<g stroke="${p.outline}" stroke-width="3" stroke-linejoin="round">
${stones}</g>
`;
  return out;
}
const COBBLE = { mortar: "#6E6B66", stones: ["#A29E97", "#B4B0A9", "#9A968F"], outline: "#5A5751" };

function mossStone(rng) {
  return (
    cobbles(rng, COBBLE) +
    blobField(rng, { count: 40, rMin: 14, rMax: 30, fill: "#6FA84A", jitter: 0.35, spacing: 0.9 }) +
    dots(rng, { count: 120, rMin: 2, rMax: 4, fill: "#8CC463" })
  );
}

function ore(rng, colour, outline, shine) {
  let out = stoneLayers(rng);
  out += `<g stroke="${outline}" stroke-width="3" stroke-linejoin="round">\n`;
  out += cells(rng, (x0, y0, r) => {
    let s = "";
    const n = 2 + Math.floor(r() * 3), cx0 = x0 + 32 + (r() - 0.5) * 14, cy0 = y0 + 32 + (r() - 0.5) * 14;
    for (let k = 0; k < n; k++) {
      const cx = cx0 + (r() - 0.5) * 22, cy = cy0 + (r() - 0.5) * 22, rad = 5 + r() * 4;
      s += `<path d="${blobPath(r, cx, cy, rad, { jitter: 0.2, points: 6, elong: 1.1 })}" fill="${colour}"/>`;
      s += `<circle cx="${f(cx - rad * 0.3)}" cy="${f(cy - rad * 0.3)}" r="${f(rad * 0.28)}" fill="${shine}" stroke="none"/>`;
    }
    return s;
  });
  return out + "</g>\n";
}

function stoneBricks(rng) {
  const p = { mortar: "#77746F", bricks: ["#A8A6A2", "#9C9A96", "#B1AFAA"], edge: "#8A8783" };
  let out = rect(p.mortar);
  const bw = 64, bh = 32, gap = 3;
  for (let row = 0; row < SIZE / bh; row++) {
    const off = row % 2 ? bw / 2 : 0;
    for (let x = -bw + off; x < SIZE; x += bw) {
      const y = row * bh;
      const brick = `<rect x="${x + gap / 2}" y="${y + gap / 2}" width="${bw - gap}" height="${bh - gap}" rx="4" fill="${pick(rng, p.bricks)}"/>` +
        `<rect x="${x + gap / 2 + 4}" y="${y + bh - gap / 2 - 5}" width="${bw - gap - 8}" height="3" rx="1.5" fill="${p.edge}"/>`;
      out += brick;
    }
    out += "\n";
  }
  return out;
}

function slate(rng) {
  const cols = ["#4F5A66", "#5B6774", "#445059", "#65717E"];
  let out = rect(cols[0]);
  for (let band = 0; band < CELLS; band++) {
    const y0 = band * CELL;
    let y = y0;
    let k = 0;
    while (y < y0 + CELL - 6) {
      const h = 10 + rng() * 12;
      const terms = waveTerms(rng, 2);
      let d = `M0 ${f(y)}`;
      for (let x = 0; x <= SIZE; x += 16) d += `L${x} ${f(wave(x, y + h, 2, rng, terms))}`;
      d += `L${SIZE} ${f(y)}Z`;
      out += `<path d="${d}" fill="${cols[(k++ % 3) + 1]}"/>\n`;
      y += h;
    }
  }
  return out;
}

function rockyDirt(rng) {
  let out = dirtLayers(rng, DIRT);
  out += `<g stroke="#6B6D70" stroke-width="3" stroke-linejoin="round">\n`;
  out += cells(rng, (x0, y0, r) => {
    let s = "";
    if (r() > 0.45) return "";
    const n = 1 + Math.floor(r() * 1.5);
    for (let k = 0; k < n; k++) {
      const cx = x0 + 14 + r() * 36, cy = y0 + 14 + r() * 36, rad = 6 + r() * 7;
      s += `<path d="${blobPath(r, cx, cy, rad, { jitter: 0.15 })}" fill="${pick(r, ["#9C9FA3", "#B0B3B7"])}"/>`;
    }
    return s;
  });
  return out + "</g>\n";
}

const WOOD = { plank: "#C8A165", seam: "#9A7440", frame: "#8C6A3A", nail: "#6B4E2A", light: "#D8B47A" };

function crateSide(rng) {
  let out = rect(WOOD.plank);
  out += cells(rng, (x0, y0, r) => {
    let s = "";
    for (let k = 1; k < 4; k++) s += `<rect x="${x0}" y="${y0 + k * 16 - 1.5}" width="${CELL}" height="3" fill="${WOOD.seam}"/>`;
    for (let k = 0; k < 4; k++) s += `<rect x="${x0 + 6}" y="${y0 + k * 16 + 3}" width="${CELL - 12}" height="2" rx="1" fill="${WOOD.light}"/>`;
    s += `<rect x="${x0 + 3}" y="${y0 + 3}" width="${CELL - 6}" height="${CELL - 6}" rx="3" fill="none" stroke="${WOOD.frame}" stroke-width="6"/>`;
    for (const [nx, ny] of [[9, 9], [55, 9], [9, 55], [55, 55]]) s += `<circle cx="${x0 + nx}" cy="${y0 + ny}" r="2.2" fill="${WOOD.nail}"/>`;
    return s;
  });
  return out;
}

function crateTop(rng) {
  let out = rect(WOOD.plank);
  out += cells(rng, (x0, y0) => {
    let s = "";
    for (let k = 1; k < 4; k++) s += `<rect x="${x0 + k * 16 - 1.5}" y="${y0}" width="3" height="${CELL}" fill="${WOOD.seam}"/>`;
    s += `<rect x="${x0 + 3}" y="${y0 + 3}" width="${CELL - 6}" height="${CELL - 6}" rx="3" fill="none" stroke="${WOOD.frame}" stroke-width="6"/>`;
    s += `<path d="M${x0 + 8} ${y0 + 8}L${x0 + 56} ${y0 + 56}M${x0 + 56} ${y0 + 8}L${x0 + 8} ${y0 + 56}" stroke="${WOOD.frame}" stroke-width="7" stroke-linecap="round"/>`;
    for (const [nx, ny] of [[9, 9], [55, 9], [9, 55], [55, 55]]) s += `<circle cx="${x0 + nx}" cy="${y0 + ny}" r="2.2" fill="${WOOD.nail}"/>`;
    return s;
  });
  return out;
}

const CLAY = { base: "#C4744A", dark: "#A65C38", light: "#DB8B60", hole: "#5C3222" };

function potTop(rng) {
  let out = rect(CLAY.base);
  out += cells(rng, (x0, y0) =>
    `<circle cx="${x0 + 32}" cy="${y0 + 32}" r="26" fill="${CLAY.light}"/>` +
    `<circle cx="${x0 + 32}" cy="${y0 + 32}" r="21" fill="${CLAY.dark}"/>` +
    `<circle cx="${x0 + 32}" cy="${y0 + 32}" r="17" fill="${CLAY.hole}"/>`);
  return out;
}

function potSide(rng) {
  let out = rect(CLAY.base);
  for (let band = 0; band < CELLS; band++) {
    const y0 = band * CELL;
    out += `<rect x="0" y="${y0}" width="${SIZE}" height="9" fill="${CLAY.dark}"/>`;
    out += `<rect x="0" y="${y0 + 9}" width="${SIZE}" height="3" fill="${CLAY.light}"/>`;
    out += `<rect x="0" y="${y0 + 26}" width="${SIZE}" height="4" fill="${CLAY.light}"/>`;
    out += `<rect x="0" y="${y0 + 54}" width="${SIZE}" height="10" fill="${CLAY.dark}"/>\n`;
  }
  out += dots(rng, { count: 80, rMin: 1.5, rMax: 3, fill: CLAY.dark });
  return out;
}

function potBottom(rng) {
  let out = rect("#B5683F");
  out += cells(rng, (x0, y0) => `<circle cx="${x0 + 32}" cy="${y0 + 32}" r="24" fill="none" stroke="${CLAY.dark}" stroke-width="5"/>`);
  return out;
}

const BARK = { base: "#6E4A2B", dark: "#553619", light: "#86593A", wood: "#D2A46B", ring: "#B8874F" };

function logSide(rng) {
  let out = rect(BARK.base);
  // Vertical bark strips: full height, periodic in y by construction; wrapped in x.
  for (let x = 0; x < SIZE; x += 9 + rng() * 10) {
    const w = 4 + rng() * 7, fill = rng() < 0.5 ? BARK.dark : BARK.light;
    const terms = waveTerms(rng, 2);
    let d = `M${f(x)} 0`;
    for (let y = 0; y <= SIZE; y += 32) d += `L${f(x + Math.sin((y / SIZE) * Math.PI * 2 * terms[0][0] + terms[0][1]) * 2.5)} ${y}`;
    for (let y = SIZE; y >= 0; y -= 32) d += `L${f(x + w + Math.sin((y / SIZE) * Math.PI * 2 * terms[1][0] + terms[1][1]) * 2.5)} ${y}`;
    d += "Z";
    out += wrapped(`<path d="${d}" fill="${fill}"/>`, [x - 4, 0, x + w + 4, SIZE]);
  }
  // A few knots.
  for (let i = 0; i < 14; i++) {
    const cx = rng() * SIZE, cy = rng() * SIZE;
    out += wrapped(`<ellipse cx="${f(cx)}" cy="${f(cy)}" rx="7" ry="11" fill="${BARK.dark}"/><ellipse cx="${f(cx)}" cy="${f(cy)}" rx="3" ry="5" fill="${BARK.light}"/>`, [cx - 8, cy - 12, cx + 8, cy + 12]);
  }
  return out;
}

function logTop(rng) {
  let out = rect(BARK.base);
  out += cells(rng, (x0, y0, r) => {
    const cx = x0 + 32, cy = y0 + 32;
    let s = `<rect x="${x0 + 2}" y="${y0 + 2}" width="60" height="60" rx="12" fill="${BARK.light}"/>`;
    s += `<circle cx="${cx}" cy="${cy}" r="24" fill="${BARK.wood}"/>`;
    for (let rad = 19; rad > 4; rad -= 6) {
      const ox = (r() - 0.5) * 3, oy = (r() - 0.5) * 3;
      s += `<circle cx="${f(cx + ox)}" cy="${f(cy + oy)}" r="${rad}" fill="none" stroke="${BARK.ring}" stroke-width="2.5"/>`;
    }
    s += `<circle cx="${cx}" cy="${cy}" r="3" fill="${BARK.ring}"/>`;
    return s;
  });
  return out;
}

function leaves(rng) {
  let out = rect("#4E9A3C");
  out += blobField(rng, { count: 30, rMin: 30, rMax: 60, fill: "#3B7A2C", jitter: 0.35, spacing: 0.9 });
  out += blobField(rng, { count: 34, rMin: 22, rMax: 44, fill: "#6DBB4A", jitter: 0.35, spacing: 0.9 });
  // Little leaf shapes.
  out += `<g fill="#5FAE45">\n`;
  for (let i = 0; i < 140; i++) {
    const cx = rng() * SIZE, cy = rng() * SIZE, a = rng() * 360, L = 9 + rng() * 6;
    out += wrapped(`<ellipse cx="${f(cx)}" cy="${f(cy)}" rx="${f(L)}" ry="${f(L * 0.45)}" transform="rotate(${f(a)} ${f(cx)} ${f(cy)})"/>`, [cx - L, cy - L, cx + L, cy + L]);
  }
  return out + "</g>\n";
}

// ---------------------------------------------------------------- registry (index = atlas tile id)

const TILES = [
  ["air", () => rect("#808080")],
  ["dirt", (r) => dirtLayers(r, DIRT)],
  ["grass_side", (r) => `<g id="dirt">${dirtLayers(r, DIRT)}</g>\n` + grassCap(r)],
  ["grass_top", grassTop],
  ["stone", (r) => stoneLayers(r)],
  ["cobblestone", (r) => cobbles(r, COBBLE)],
  ["moss_stone", mossStone],
  ["iron_ore", (r) => ore(r, "#D9B08C", "#A57A55", "#F2DCC3")],
  ["copper_ore", (r) => ore(r, "#E08A4A", "#A35A2A", "#F5BE8E")],
  ["diamond_ore", (r) => ore(r, "#7FE3E8", "#3FA5AC", "#D6FBFC")],
  ["coal_ore", (r) => ore(r, "#3B3B3F", "#1E1E22", "#6C6C72")],
  ["ruby_ore", (r) => ore(r, "#E24C6A", "#9B2340", "#F7A3B4")],
  ["stone_bricks", stoneBricks],
  ["slate", slate],
  ["tough_dirt", (r) => dirtLayers(r, TOUGH) + cracks(r, 24, "#4E3116")],
  ["rocky_dirt", rockyDirt],
  ["crate_side", crateSide],
  ["crate_top", crateTop],
  ["pot_top", potTop],
  ["pot_side", potSide],
  ["pot_bottom", potBottom],
  ["log_side", logSide],
  ["leaves", leaves],
  ["log_top", logTop],
];

// ---------------------------------------------------------------- render

const only = (() => {
  const i = process.argv.indexOf("--only");
  return i > 0 ? new Set(process.argv[i + 1].split(",").map(Number)) : null;
})();

mkdirSync(SVG_DIR, { recursive: true });

const stacked = new PNG({ width: SIZE, height: SIZE * TILES.length });
const sheetCols = 6, thumb = 256;
const sheet = new PNG({ width: sheetCols * thumb, height: Math.ceil(TILES.length / sheetCols) * thumb });

function blit(dst, src, dx, dy) {
  for (let y = 0; y < src.height; y++) src.data.copy(dst.data, ((dy + y) * dst.width + dx) * 4, (y * src.width) * 4, (y * src.width + src.width) * 4);
}

TILES.forEach(([name, build], idx) => {
  if (only && !only.has(idx)) return;
  const rng = mulberry32(0x9e3779b9 ^ (idx * 0x85ebca6b));
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${SIZE}" height="${SIZE}" viewBox="0 0 ${SIZE} ${SIZE}">\n${build(rng)}</svg>\n`;
  writeFileSync(join(SVG_DIR, `${String(idx).padStart(2, "0")}_${name}.svg`), svg);
  const full = PNG.sync.read(new Resvg(svg).render().asPng());
  for (let i = 0; i < full.data.length; i += 4) {
    full.data[i] = Math.round(full.data[i] * ALBEDO_SCALE);
    full.data[i + 1] = Math.round(full.data[i + 1] * ALBEDO_SCALE);
    full.data[i + 2] = Math.round(full.data[i + 2] * ALBEDO_SCALE);
  }
  blit(stacked, full, 0, idx * SIZE);
  const small = PNG.sync.read(new Resvg(svg, { fitTo: { mode: "width", value: thumb } }).render().asPng());
  blit(sheet, small, (idx % sheetCols) * thumb, Math.floor(idx / sheetCols) * thumb);
  process.stdout.write(`${idx} ${name}\n`);
});

if (!only) {
  writeFileSync(join(ASSETS, "blocks_mega.png"), PNG.sync.write(stacked, { deflateLevel: 9 }));
}
// Only a full run produces a complete sheet; a subset would blank the rest.
if (!only) writeFileSync(join(ROOT, "scripts", "textures", "contact_sheet.png"), PNG.sync.write(sheet));
console.log(`layers=${TILES.length} ${only ? "(subset; stacked PNG not written)" : "wrote assets/blocks_mega.png"}`);
